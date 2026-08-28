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
//! * **No retry for the UN-WITNESSED verbs.** S8's owner-side dedup window
//!   is what makes a resend safe, and the one absolutely non-idempotent
//!   verb (`create_with_rdev_size`) would create a second name on a resend
//!   after a lost reply — so un-witnessed transport failures are REPORTED,
//!   never re-applied; the writeback ladder above re-publishes from
//!   current state and converges. **The layout-publish class
//!   (`SetLayoutAndSize` / `MergeLayoutAndSize` / `CommitBlockRefs`)
//!   joined the RETRIED class at schema 5** (finding #6,
//!   `docs/design-mw-layout-versions.md` §6a — the shape PR 17 schedules
//!   for `WriteExtent`): it carries the `(lease_epoch, request_id)`
//!   witness, resends the SAME frame only (bounded, epoch-stable, never
//!   re-keyed — `ship_witnessed`), and a duplicate answers the winner's
//!   own cached outcome, because the pre-witness composition of no-retry
//!   with the never-lossy ladder was exactly the divergent-chain mint the
//!   s9-colocated-fence leg convicted. **Every mutating verb is also
//!   ERA-GATED** (`lease_epoch`, refused `PUBLISH_STALE_LEASE` before the
//!   window): a swept-but-not-yet-self-fenced zombie's publishes refuse
//!   with nothing applied, and a current-epoch refusal composes the
//!   client's full fence (the pull-based revocation law).
//! * **No batching conveyor.** The ops that arrive here are already
//!   coalesced (the M7 commit conveyor, the publish coalescer's
//!   `publish_commit_group*`), so a second batching layer would coalesce
//!   already-coalesced work. S8's lane shape is the precedent if a measured
//!   row ever asks for one.
//! * **No cross-owner transaction.** `destroy_inodes` is per-ino by
//!   construction and groups by owner; nothing here spans two authorities
//!   inside one transaction (that is S3.5, and S8's `cross_owner_error` is
//!   the refusal wherever it can be reached).

use super::service::DedupWindow;
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
///
/// **3 since the co-writer FREE path landed** (`FreeBlocks` +
/// [`PublishReply::FreeVerdicts`] joined). Same reasoning: a peer that
/// speaks 2 cannot serve a shipped free, and a co-writer whose displaced
/// frees silently vanished would leak every rewritten block until the
/// authority's next recovery.
///
/// **4 since the lane free HARVEST landed** (`HarvestLaneFree` +
/// [`PublishReply::LaneFreeGrant`] joined — rung 10, residual 2): a peer
/// that speaks 3 cannot hand a co-writer its lane's freed supply back, and
/// a co-writer whose harvests silently vanished would starve `StorageFull`
/// on a store with free space.
///
/// **5 since the era gate + layout-publish witness landed** (finding #6,
/// `docs/design-mw-layout-versions.md` §6a): every MUTATING call carries
/// the caller's custody `lease_epoch` (refused [`PUBLISH_STALE_LEASE`]
/// when it is not live custody — the FreeBlocks gate, generalized), and
/// the layout-publish class (`SetLayoutAndSize` / `MergeLayoutAndSize` /
/// `CommitBlockRefs`) also carries `request_id`, the exactly-once witness.
/// A 4-speaker's layout publishes would be un-gated and un-witnessed — the
/// divergent-chain mint — so the honest answer is `PUBLISH_SCHEMA_MISMATCH`
/// at the first frame.
///
/// **6 since the AUTHORITY ASSEMBLER landed** (DLM S11 rung 17, KD-MW-8
/// revised — `docs/design-full-multi-writer.md` §9.3; the design text
/// says "publish schema 4", written before HarvestLaneFree took 3→4 and
/// finding #6 took 4→5 — the CONTENT governs, adjudicated in the rung's
/// evidence note): `WriteExtent` (a sub-block-shared block's bytes,
/// shipped to the authority that assembles and publishes as the SINGLE
/// publisher) + `FlushExtents` (the fsync force) joined, and the
/// `MergeLayoutAndSize` reply gained the STAGED LINK'S VERSION
/// ([`PublishReply::DeltaUsed`] became a struct variant) — the
/// chain-without-refetch input a 5-speaker cannot decode.
///
/// **7 since the owner-partitioned block-ref population read landed**
/// (finding 13, per-volume claim admission PR 8): `BlockRefPopulation` +
/// [`PublishReply::Populations`] joined — the shipped-free validation
/// reads the ledger where the ledger LIVES (each volume's owner), because
/// a 6-speaker set authority answered a peer's shipped free from its own
/// lagged snapshot of the peer's volume (the `NonTerminal` strand / false
/// `Freed` pair `tests/pv_shipped_free_ledger_tests.rs` pins).
///
/// **8 since the harvest reply carries the authority's bound age** (the
/// free-grace sustain campaign PR 4, OQ 2 — user decision 2026-08-25):
/// [`PublishReply::LaneFreeGrant`] became a struct variant whose
/// `bound_age_ms` is the authority's live loop latency, so the
/// co-writer's ahead-refill horizon is a MEASUREMENT instead of a
/// derivation. A 7-speaker cannot decode the widened reply — the
/// mismatch refuses loud in both directions (this wire's standing
/// posture), and the co-writer's horizon word then simply keeps its
/// derivation default, which is the design's fallback by construction.
pub const PUBLISH_SCHEMA: u32 = 8;

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
/// Status: a FREE verb presented a lease epoch that is not custody on this
/// authority (revoked, swept, or minted by a previous era) — the fencing
/// refusal of the co-writer free path (`free_stale_refusals`). Refused
/// BEFORE the dedup window on purpose: a dead era's replay must never be
/// answered from a cached outcome, and a dead era's first attempt must
/// never execute.
pub const PUBLISH_STALE_LEASE: u16 = 0x56;

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
    /// **Layout-publish class** (finding #6, design-mw-layout-versions §6a):
    /// carries the era gate's input (`lease_epoch`) AND the exactly-once
    /// witness's other half (`request_id`) — this class is RETRIED
    /// (same-frame-only, bounded, epoch-stable; see the module docs), which
    /// only the owner's dedup window makes safe.
    SetLayoutAndSize {
        ino: u64,
        layout: Vec<u8>,
        size: u64,
        refs: Vec<WireBlockRefOp>,
        /// The custody lease epoch the caller holds — the era gate's input
        /// and half of the idempotence witness.
        lease_epoch: u64,
        /// Client-chosen, monotone per process — the witness's other half.
        /// ONE id per logical publish; a resend never re-keys.
        request_id: u64,
    },
    /// The delta travels **encoded** (`LayoutDelta::encode`), which is the
    /// same strict, bounded, magic-checked codec the on-disk record uses —
    /// so the wire cannot express a delta the volume could not store.
    /// Layout-publish class: era-gated + witnessed (see `SetLayoutAndSize`).
    MergeLayoutAndSize {
        ino: u64,
        delta: Vec<u8>,
        full_layout: Vec<u8>,
        size: u64,
        refs: Vec<WireBlockRefOp>,
        /// Era gate input + witness half (see `SetLayoutAndSize`).
        lease_epoch: u64,
        /// The witness's other half (see `SetLayoutAndSize`).
        request_id: u64,
    },
    /// Layout-publish class: era-gated + witnessed (see `SetLayoutAndSize`).
    CommitBlockRefs {
        ino: u64,
        refs: Vec<WireBlockRefOp>,
        /// Era gate input + witness half (see `SetLayoutAndSize`).
        lease_epoch: u64,
        /// The witness's other half (see `SetLayoutAndSize`).
        request_id: u64,
    },
    /// Era-gated, un-witnessed and un-retried: absolute times are
    /// last-writer-wins, so a lost reply converges without a window.
    ParkWriteTimes {
        ino: u64,
        mtime: u64,
        ctime: u64,
        /// The era gate's input (finding #6 — a zombie parks nothing).
        lease_epoch: u64,
    },
    /// Era-gated, un-witnessed and un-retried (per-ino teardown).
    DestroyInodes {
        inos: Vec<u64>,
        /// The era gate's input.
        lease_epoch: u64,
    },
    /// Era-gated; keeps the no-retry law ABSOLUTELY (a resend after a lost
    /// reply mints a second name — the doctrine's founding case).
    CreateWithRdevSize {
        parent: u64,
        name: String,
        mode: u32,
        uid: u32,
        gid: u32,
        rdev: u32,
        initial_size: u64,
        /// The era gate's input.
        lease_epoch: u64,
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
    /// **Release displaced blocks' ACCOUNTING** — the co-writer FREE path
    /// (DLM S9; contracts `tests/mw_cowriter_free_tests.rs`).
    ///
    /// The durable half of a free — the `TREE_BLOCK_REFS` delete — already
    /// rode the layout publish that displaced the block, so this verb
    /// carries only what is left: the request that the AUTHORITY run its
    /// own free ladder (`begin_free` → tier purge → reclaim enqueue →
    /// `finish_free`, with the §6.8 item-3 grace ring and S7's quarantine
    /// composing inside `finish_free` exactly as they do for a local
    /// free). Frees are lane-blind (`b % W` derives the owner), so unlike
    /// `RaiseAllocLane` there is no lane check — the owner-side validation
    /// is the DURABLE LEDGER itself: a block the ledger still references
    /// is answered `NonTerminal` and nothing moves.
    ///
    /// **This verb is retried, and that is safe here alone**: the owner
    /// keys a dedup window on `(lease_epoch, request_id)` — S8's window,
    /// reused — so a resend after a lost reply answers the winner's own
    /// cached outcome. `lease_epoch` is minted by the authority, monotone
    /// and never reused, and a retry NEVER re-keys under a fresh epoch (a
    /// re-joined mount abandons its old-epoch frees, which the era gate
    /// refuses anyway) — that is what closes the freed-then-reallocated
    /// ABA window a cross-epoch retry would open.
    FreeBlocks {
        /// The durable data-volume identity (KD-5, as above).
        vol_tag: u64,
        /// Dense block indices (`offset / chunk`) — the identity
        /// `TREE_BLOCK_REFS` keys on, never device offsets or key strings.
        blocks: Vec<u64>,
        /// The custody lease epoch the caller holds — the era gate's input
        /// and half of the idempotence witness.
        lease_epoch: u64,
        /// Client-chosen, monotone per process — the witness's other half.
        request_id: u64,
    },
    /// **Harvest the caller's lane's freed supply** — rung 10's residual-2
    /// seam, the reuse half of the co-writer FREE path.
    ///
    /// A shipped free re-enters "the free supply of lane `b % W`" — the
    /// AUTHORITY's free list, whose own allocation funnel is lane-filtered.
    /// Without this verb that supply is reachable by NOBODY: the co-writer's
    /// allocator is frontier-monotone (the rung-9 named residual), so
    /// sustained rewrite leaks toward ENOSPC on a store with free space.
    ///
    /// The owner hands back up to `max` free-listed block indices of the
    /// CALLER's lane, **removing them from its own free list** (exactly-once
    /// — nobody can receive an offset twice), draining its reclaim queue
    /// first when the lane's supply is still queued, and recording every
    /// handout against `lease_epoch` so an epoch that dies with the
    /// reference not yet durable quarantines them (the §3.1 zombie window
    /// closed for REUSED offsets the way the durable frontier closes it for
    /// fresh mints; the record discharges on the offset's next shipped
    /// free). Lane + lease are validated against the assignment THIS
    /// authority made, exactly as `RaiseAllocLane`'s are.
    ///
    /// **Retried like `FreeBlocks` and only like it**: the owner keys the
    /// same dedup-window pattern on `(lease_epoch, request_id)` — a resend
    /// after a lost reply answers the winner's own grant, and a retry never
    /// re-keys across a re-join.
    HarvestLaneFree {
        /// The durable data-volume identity (KD-5, as above).
        vol_tag: u64,
        lane: u16,
        writers: u16,
        /// Handout cap, blocks (the caller's reservation grain — derived,
        /// never a knob).
        max: u64,
        /// The custody lease epoch the caller holds — the era gate's input,
        /// half of the idempotence witness, and the handout ledger's key.
        lease_epoch: u64,
        /// Client-chosen, monotone per process — the witness's other half.
        request_id: u64,
    },
    /// **One sub-block extent of a SHARED block, shipped to the
    /// AUTHORITY** — DLM S11 rung 17 (KD-MW-8 revised, design §9.3): a
    /// block two live range grants share demotes to authority-assembled,
    /// and BOTH holders' writes to it travel here (the W2 extent-record
    /// vocabulary: offset-in-block, bytes, fencing token). The authority
    /// merges via the existing extent overlay/fold and publishes once —
    /// the SINGLE publisher for the block.
    ///
    /// **Joins the RETRIED class — stated against this module's no-retry
    /// doctrine** (the module docs' opening law): like the layout-publish
    /// class at schema 5 and the free/harvest verbs before it, the
    /// `(lease_epoch, request_id)` witness is exactly what makes a
    /// same-frame resend safe (a lost-reply retry answers the winner's
    /// own ack), and the era gate refuses a dead epoch BEFORE the window.
    WriteExtent {
        ino: u64,
        /// The block's index (`offset / block_size`) — the assembler's
        /// merge key.
        block_index: u64,
        /// Offset INSIDE the block (the W2 extent vocabulary).
        offset_in_block: u32,
        data: Vec<u8>,
        /// The shipper's own range-grant fencing token (audit; the era
        /// gate's authority is `lease_epoch`).
        token: u64,
        /// Era gate input + witness half.
        lease_epoch: u64,
        /// The witness's other half.
        request_id: u64,
    },
    /// **The fsync force** (rung 17, §9.3's retention law): fold and
    /// publish every extent the authority holds for `ino`, answering the
    /// covering layout version — a sub-block writer's fsync chains
    /// through the AUTHORITY's publish barrier for every retained extent
    /// (the shipped-free precedent's synchronous form). Witnessed and
    /// retried (idempotent: a duplicate flush re-answers the winner's
    /// covering version).
    FlushExtents {
        ino: u64,
        /// Era gate input + witness half.
        lease_epoch: u64,
        /// The witness's other half.
        request_id: u64,
    },
    /// **The durable reference population of blocks, answered from the
    /// volumes the SERVING node owns** — the owner-partitioned read half
    /// of the shipped-free validation (finding 13, per-volume claim
    /// admission PR 8; contracts `tests/pv_shipped_free_ledger_tests.rs`).
    ///
    /// Under D20 the `TREE_BLOCK_REFS` ledger is DISTRIBUTED: a partial
    /// authority's layout publishes commit on its OWN volumes, so a peer's
    /// copy of those volumes is a lagged reader snapshot, and validating a
    /// shipped free against it answers from state the owner has already
    /// moved — a released reference still counted (`NonTerminal` forever,
    /// the leg's ship=128/serve=0 strand) or a fresh reference invisible
    /// (a false `Freed`, §6.3's destructive face). The read therefore
    /// SHIPS to each volume's owner, and the serve answers for its OWNED
    /// volumes only — the reply is scoped, never refused, which is why
    /// this verb names no inos.
    ///
    /// Pure read (the `XattrValueCap`/`ReaddirStream` class): no era
    /// gate, no witness, transport-resend-safe.
    BlockRefPopulation {
        /// The durable data-volume identity (KD-5, as above).
        vol_tag: u64,
        /// Dense block indices — `TREE_BLOCK_REFS`'s own key grain.
        block_idxs: Vec<u64>,
    },
}

/// One block's outcome inside a served [`PublishCall::FreeBlocks`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FreeVerdict {
    /// The terminal release ran: the authority's ladder owns the offset
    /// through reclaim, and it re-enters the free supply of the lane the
    /// arithmetic names.
    Freed,
    /// The durable ledger (or the authority's live refcount) still holds
    /// references: the release already happened durably on the publish,
    /// and nothing else is owed.
    NonTerminal,
    /// The block is already free / in grace / quarantined / mid-reclaim —
    /// the double-release lineage, refused LOUD on the authority's own
    /// untracked-free tripwire (never a silent second free).
    Refused,
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
            Self::FreeBlocks { .. } => "free_blocks",
            Self::HarvestLaneFree { .. } => "harvest_lane_free",
            Self::WriteExtent { .. } => "write_extent",
            Self::FlushExtents { .. } => "flush_extents",
            Self::BlockRefPopulation { .. } => "block_ref_population",
        }
    }

    /// The lease epoch a MUTATING call presents — `None` for the read
    /// verbs (`XattrValueCap` / `ReaddirStream`), which mutate nothing (a
    /// zombie's stale read is the S5 reader-staleness class, never a
    /// durability threat). This is the CLIENT-side fence-composition
    /// input; the owner-side gates are matched explicitly in `serve` so
    /// the free/harvest/raise verbs keep their own gate order and counters.
    pub fn presented_epoch(&self) -> Option<u64> {
        match self {
            Self::SetLayoutAndSize { lease_epoch, .. }
            | Self::MergeLayoutAndSize { lease_epoch, .. }
            | Self::CommitBlockRefs { lease_epoch, .. }
            | Self::ParkWriteTimes { lease_epoch, .. }
            | Self::DestroyInodes { lease_epoch, .. }
            | Self::CreateWithRdevSize { lease_epoch, .. }
            | Self::RaiseAllocLane { lease_epoch, .. }
            | Self::FreeBlocks { lease_epoch, .. }
            | Self::HarvestLaneFree { lease_epoch, .. }
            | Self::WriteExtent { lease_epoch, .. }
            | Self::FlushExtents { lease_epoch, .. } => Some(*lease_epoch),
            Self::XattrValueCap { .. }
            | Self::ReaddirStream { .. }
            | Self::BlockRefPopulation { .. } => None,
        }
    }

    /// The generic era gate's input (design §6a law 1): the six mutating
    /// verbs that gained `lease_epoch` at schema 5, plus rung 17's extent
    /// pair (whose refusals land on their OWN counter —
    /// `extent_stale_refusals`). The raise/free/harvest verbs are
    /// deliberately EXCLUDED — they run their own, older gates in `serve`
    /// (lane validation / `validate_free`) with their own counters.
    fn era_gated_epoch(&self) -> Option<u64> {
        match self {
            Self::SetLayoutAndSize { lease_epoch, .. }
            | Self::MergeLayoutAndSize { lease_epoch, .. }
            | Self::CommitBlockRefs { lease_epoch, .. }
            | Self::ParkWriteTimes { lease_epoch, .. }
            | Self::DestroyInodes { lease_epoch, .. }
            | Self::CreateWithRdevSize { lease_epoch, .. }
            | Self::WriteExtent { lease_epoch, .. }
            | Self::FlushExtents { lease_epoch, .. } => Some(*lease_epoch),
            _ => None,
        }
    }

    /// Rung 17's extent class (its own ledger rows).
    fn is_extent(&self) -> bool {
        matches!(self, Self::WriteExtent { .. } | Self::FlushExtents { .. })
    }

    /// Does a SERVE of this call mutate an ino's durable LAYOUT plane
    /// directly on the backend (rung 18 — the served-layout invalidation
    /// sink's class)? The extent verbs are excluded: their executors run
    /// the authority fs's own write path, which keeps its RAM view
    /// coherent by construction.
    fn serves_mutate_layout(&self) -> bool {
        matches!(
            self,
            Self::SetLayoutAndSize { .. }
                | Self::MergeLayoutAndSize { .. }
                | Self::CommitBlockRefs { .. }
                | Self::DestroyInodes { .. }
        )
    }

    /// May a TRANSPORT-failed ship of this call reconnect and RESEND the
    /// same frame (rung 18, residual (d) — the idle-reap conviction: the
    /// cluster wire's 60 s idle-session reaper closed a co-writer's
    /// publish session between rows and the next lane raise refused
    /// instead of reconnecting, surfacing as EINVAL on a healthy fleet)?
    ///
    /// Resend-safe classes, each with its own exactly-once argument:
    /// * the **witnessed** class — the owner's `(lease_epoch,
    ///   request_id)` window absorbs a duplicate (design §6a law 2);
    /// * **`FreeBlocks`** — the RETRIED class by rung 9's law (its own
    ///   dedup window + per-block verdicts);
    /// * **`RaiseAllocLane` / `HarvestLaneFree`** — MONOTONE: the
    ///   frontier only rises and a harvest re-serves from the authority's
    ///   own free list, so a duplicate re-answers the standing state;
    /// * the **pure reads** (`XattrValueCap` / `ReaddirStream`) — they
    ///   mutate nothing.
    ///
    /// Everything else (the un-witnessed mutators: create/destroy/times)
    /// keeps ONE attempt — a resend after a lost reply is a second act no
    /// window can correlate (a resent create mints a second name).
    fn transport_resend_safe(&self) -> bool {
        self.witness().is_some()
            || matches!(
                self,
                Self::FreeBlocks { .. }
                    | Self::RaiseAllocLane { .. }
                    | Self::HarvestLaneFree { .. }
                    | Self::XattrValueCap { .. }
                    | Self::ReaddirStream { .. }
                    | Self::BlockRefPopulation { .. }
            )
    }

    /// The layout-publish class's exactly-once witness key
    /// `(lease_epoch, request_id)`, `None` for every un-witnessed verb.
    /// Rung 17: the extent class rides the SAME window (its replies are
    /// [`PublishReply`]s and witness ids come from ONE client sequence,
    /// so the id spaces cannot collide).
    fn witness(&self) -> Option<(u64, u64)> {
        match self {
            Self::SetLayoutAndSize {
                lease_epoch,
                request_id,
                ..
            }
            | Self::MergeLayoutAndSize {
                lease_epoch,
                request_id,
                ..
            }
            | Self::CommitBlockRefs {
                lease_epoch,
                request_id,
                ..
            }
            | Self::WriteExtent {
                lease_epoch,
                request_id,
                ..
            }
            | Self::FlushExtents {
                lease_epoch,
                request_id,
                ..
            } => Some((*lease_epoch, *request_id)),
            _ => None,
        }
    }

    /// Every inode the call names — the authority check's input.
    pub fn named_inos(&self) -> Vec<u64> {
        match self {
            Self::SetLayoutAndSize { ino, .. }
            | Self::MergeLayoutAndSize { ino, .. }
            | Self::CommitBlockRefs { ino, .. }
            | Self::ParkWriteTimes { ino, .. }
            | Self::WriteExtent { ino, .. }
            | Self::FlushExtents { ino, .. }
            | Self::XattrValueCap { ino } => vec![*ino],
            Self::DestroyInodes { inos, .. } => inos.clone(),
            Self::CreateWithRdevSize { parent, .. } => vec![*parent],
            Self::ReaddirStream { dir, .. } => vec![*dir],
            // The reservation record lives on ino 1 (KD-2's plane), so the
            // authority check is the same check every other verb gets: the
            // node serving it must hold authority over the volume ino 1
            // routes to. The FREE verb keys on the same plane: block
            // ownership accounting is set-level state, and the node that
            // owns ino 1's volume is the D0 claim holder whose ladder runs.
            Self::RaiseAllocLane { .. }
            | Self::FreeBlocks { .. }
            | Self::HarvestLaneFree { .. } => {
                vec![1]
            }
            // The reply is SCOPED to the serving node's owned volumes by
            // construction (finding 13), so the authority screen is
            // inapplicable: any owner answers for what it owns.
            Self::BlockRefPopulation { .. } => Vec::new(),
        }
    }
}

/// One publish call's successful payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PublishReply {
    Unit,
    /// `merge_layout_and_size`: whether a delta record was staged, and
    /// the STAGED LINK'S VERSION (0 on a full-Put commit) — rung 17's
    /// chain-without-refetch input (design-mw-layout-versions §6's named
    /// residual): the co-writer stamps its RAM provenance from the reply
    /// so its next delta claims the right base with no round trip.
    DeltaUsed {
        used: bool,
        version: u64,
    },
    /// `write_extent` (rung 17): `covering_version` is `Some` **iff the
    /// covering publish has already run** — releasing the shipper's
    /// retention at the round trip; `None` retains (release rides the
    /// pull surfaces — §9.3's retention law).
    ExtentAck {
        covering_version: Option<u64>,
    },
    /// `flush_extents` (rung 17): the covering layout version after the
    /// forced fold+publish — the fsync barrier's answer.
    FlushDone {
        covering_version: u64,
    },
    /// `create_with_rdev_size`.
    Inode(WireInode),
    /// `xattr_value_cap`.
    Cap(u64),
    /// `readdir_stream`: `(resume cookie, entry)` pairs.
    Page(Vec<(u64, WireDirEntry)>),
    /// `raise_alloc_lane`: the durable reservation frontier now in force for
    /// that lane — an exclusive block-index bound this lane may mint below.
    LaneFrontier(u64),
    /// `free_blocks`: one verdict per shipped block, in request order.
    FreeVerdicts(Vec<FreeVerdict>),
    /// `harvest_lane_free`: the handed-out block indices — free-listed
    /// offsets of the CALLER's lane, removed from the authority's own
    /// list (exactly-once) and recorded against the caller's lease epoch.
    /// Since schema 8 the reply also carries the authority's live
    /// `free_grace_bound_age_ms` (0 = nothing held), so the co-writer's
    /// refill horizon reads the loop latency actually in force (OQ 2).
    LaneFreeGrant {
        blocks: Vec<u64>,
        bound_age_ms: u64,
    },
    /// `block_ref_population`: per-index reference populations summed over
    /// the SERVING node's owned volumes, in request order.
    Populations(Vec<u64>),
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
// Finding #6 (design-mw-layout-versions §6a): the generic era gate's
// refusals and the layout-publish witness's replay hits.
static STALE_REFUSALS: AtomicU64 = AtomicU64::new(0);
static REPLAYS: AtomicU64 = AtomicU64::new(0);
// The co-writer FREE path's own rows (DLM S9; every one is 0 on every
// shipped mount by construction — nothing installs the verb's halves).
static FREE_SHIPPED_BLOCKS: AtomicU64 = AtomicU64::new(0);
static FREE_SERVED_BLOCKS: AtomicU64 = AtomicU64::new(0);
static FREE_REPLAYS: AtomicU64 = AtomicU64::new(0);
static FREE_STALE_REFUSALS: AtomicU64 = AtomicU64::new(0);
static FREE_SHIP_FAILURES: AtomicU64 = AtomicU64::new(0);
static HARVEST_SHIPPED_BLOCKS: AtomicU64 = AtomicU64::new(0);
static HARVEST_SERVED_BLOCKS: AtomicU64 = AtomicU64::new(0);
static HARVEST_REPLAYS: AtomicU64 = AtomicU64::new(0);
static HARVEST_REFUSALS: AtomicU64 = AtomicU64::new(0);
// Rung 17 — the extent-assembler ledger (design §13's
// `publish.extent_*` family; shipped ≡ served is the engagement law).
static EXTENT_SHIPPED: AtomicU64 = AtomicU64::new(0);
static EXTENT_SERVED: AtomicU64 = AtomicU64::new(0);
static EXTENT_REPLAYS: AtomicU64 = AtomicU64::new(0);
static EXTENT_STALE_REFUSALS: AtomicU64 = AtomicU64::new(0);
static EXTENT_FLUSH_FORCES: AtomicU64 = AtomicU64::new(0);
static EXTENT_SPILLS: AtomicU64 = AtomicU64::new(0);

/// The at-budget W2 spill's counter (incremented by
/// [`crate::extent_ship`]'s spill arm — release path 4's engagement).
pub(crate) fn note_extent_spill() {
    EXTENT_SPILLS.fetch_add(1, Ordering::Relaxed);
}

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
    /// Mutating publish verbs refused BY ERA (the presented lease epoch is
    /// not live custody) — finding #6's gate. 0 on a healthy fleet; growth
    /// around a revocation is the gate composing (a swept zombie's layout
    /// publishes refusing instead of minting a divergent chain).
    pub stale_refusals: u64,
    /// Layout-publish verbs answered from the `(lease_epoch, request_id)`
    /// witness instead of re-applied — a lost-reply retry landing here is
    /// the mechanism WORKING, not a fault.
    pub replays: u64,
    /// Displaced blocks whose FREE travelled as a verb (client side) — the
    /// co-writer rewrite path's engagement instrument: on a rewriting
    /// co-writer this tracks its displaced-block count, and 0 beside a
    /// growing `layout_publish` stream means displaced frees are leaking.
    pub free_shipped_blocks: u64,
    /// Blocks whose terminal ladder actually RAN for a peer (owner side) —
    /// `Freed` verdicts only, so shipped − served − non-terminal − refused
    /// closes per row.
    pub free_served_blocks: u64,
    /// Free verbs answered from the dedup window instead of re-applied —
    /// the exactly-once witness's engagement (a lost-reply retry landing
    /// here is the mechanism WORKING, not a fault).
    pub free_replays: u64,
    /// Free verbs refused by ERA (the presented lease epoch is not
    /// custody) — the fencing gate's row. Growth on a healthy co-writer is
    /// 0; growth around a revocation is the gate composing.
    pub free_stale_refusals: u64,
    /// Shipped frees ABANDONED by the client (transport exhausted, or the
    /// lease epoch died under the retry): each is a durably-free offset
    /// that stays out of every free list until the authority's next
    /// derivation (mount recovery / fsck C6) — the leak-safe direction,
    /// but **≈ 0** is the healthy reading.
    pub free_ship_failures: u64,
    /// Block indices a co-writer received back through the lane free
    /// HARVEST (client side) — the rung-10 reuse-engagement instrument: a
    /// sustained-rewrite row whose displaced blocks exceed the lane share
    /// must grow this, or the mount is burning frontier.
    pub harvest_shipped_blocks: u64,
    /// Block indices HANDED OUT by this authority (owner side) — each one
    /// removed from its own free list and recorded against the caller's
    /// lease epoch until discharged by the offset's next shipped free.
    pub harvest_served_blocks: u64,
    /// Harvest verbs answered from the dedup window (the lost-reply retry
    /// landing here is the mechanism working).
    pub harvest_replays: u64,
    /// Harvest verbs refused — stale era, a lane the caller was not
    /// assigned, or a width this era does not run. **Must stay 0** on a
    /// healthy fleet; growth around a revocation is the gate composing.
    pub harvest_refusals: u64,
    /// Extents SHIPPED to an authority (client side) — the sub-block
    /// exception row's engagement instrument: every sub-block write to a
    /// shared block must account here (both holders — the demotion is
    /// symmetric).
    pub extent_shipped: u64,
    /// Extents MERGED by this authority's assembler (owner side) —
    /// `shipped ≡ served` is the engagement law.
    pub extent_served: u64,
    /// Extent verbs answered from the witness window (a lost-reply retry
    /// landing here is the mechanism WORKING).
    pub extent_replays: u64,
    /// Extent verbs refused BY ERA (the extent face of `stale_refusals`;
    /// 0 on a healthy fleet).
    pub extent_stale_refusals: u64,
    /// `FlushExtents` fsync-force RPCs shipped.
    pub extent_flush_forces: u64,
    /// At-budget W2 spills of retained extents (release path 4).
    pub extent_spills: u64,
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
        stale_refusals: STALE_REFUSALS.load(Ordering::Relaxed),
        replays: REPLAYS.load(Ordering::Relaxed),
        free_shipped_blocks: FREE_SHIPPED_BLOCKS.load(Ordering::Relaxed),
        free_served_blocks: FREE_SERVED_BLOCKS.load(Ordering::Relaxed),
        free_replays: FREE_REPLAYS.load(Ordering::Relaxed),
        free_stale_refusals: FREE_STALE_REFUSALS.load(Ordering::Relaxed),
        free_ship_failures: FREE_SHIP_FAILURES.load(Ordering::Relaxed),
        harvest_shipped_blocks: HARVEST_SHIPPED_BLOCKS.load(Ordering::Relaxed),
        harvest_served_blocks: HARVEST_SERVED_BLOCKS.load(Ordering::Relaxed),
        harvest_replays: HARVEST_REPLAYS.load(Ordering::Relaxed),
        harvest_refusals: HARVEST_REFUSALS.load(Ordering::Relaxed),
        extent_shipped: EXTENT_SHIPPED.load(Ordering::Relaxed),
        extent_served: EXTENT_SERVED.load(Ordering::Relaxed),
        extent_replays: EXTENT_REPLAYS.load(Ordering::Relaxed),
        extent_stale_refusals: EXTENT_STALE_REFUSALS.load(Ordering::Relaxed),
        extent_flush_forces: EXTENT_FLUSH_FORCES.load(Ordering::Relaxed),
        extent_spills: EXTENT_SPILLS.load(Ordering::Relaxed),
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
        "stale_refusals": s.stale_refusals,
        "replays": s.replays,
        "free_shipped_blocks": s.free_shipped_blocks,
        "free_served_blocks": s.free_served_blocks,
        "free_replays": s.free_replays,
        "free_stale_refusals": s.free_stale_refusals,
        "free_ship_failures": s.free_ship_failures,
        "harvest_shipped_blocks": s.harvest_shipped_blocks,
        "harvest_served_blocks": s.harvest_served_blocks,
        "harvest_replays": s.harvest_replays,
        "harvest_refusals": s.harvest_refusals,
        "extent_shipped": s.extent_shipped,
        "extent_served": s.extent_served,
        "extent_replays": s.extent_replays,
        "extent_stale_refusals": s.extent_stale_refusals,
        "extent_flush_forces": s.extent_flush_forces,
        "extent_spills": s.extent_spills,
        // §9.3's live retention gauge (→ 0 at quiesce — falsifiable
        // against the four release paths).
        "extent_retained_bytes": crate::extent_ship::retained_bytes(),
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
    sessions: scc::HashMap<String, Arc<crate::sqz_sync::SqzMutex<Option<RpcClient>>>>,
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

    fn lane(&self, endpoint: &str) -> Arc<crate::sqz_sync::SqzMutex<Option<RpcClient>>> {
        if let Some(lane) = self.sessions.read_sync(endpoint, |_, l| Arc::clone(l)) {
            return lane;
        }
        let lane = Arc::new(crate::sqz_sync::SqzMutex::new(None));
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
    /// **One attempt, deliberately** — see the module docs: un-witnessed
    /// verbs have no dedup window, so a resend of `create_with_rdev_size`
    /// after a lost reply would mint a second name. The witnessed
    /// layout-publish class rides [`ship_witnessed`], whose bounded
    /// epoch-stable ladder resends the SAME frame only.
    ///
    /// A [`PUBLISH_STALE_LEASE`] answer is the era gate firing (finding
    /// #6): when the refused epoch IS this client's current lease, the
    /// full fence composes HERE (`note_publish_era_refused` — the
    /// pull-based revocation law at the publish round trip), and the
    /// error surfaces in the fence class (`WriterGuardFenced`) every
    /// retry ladder returns immediately.
    pub async fn ship(&self, endpoint: &str, call: PublishCall) -> Result<PublishReply> {
        let name = call.name();
        let presented = call.presented_epoch();
        let resend_safe = call.transport_resend_safe();
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
        // Finding 14 (the width-8 re-grade's conviction): a POOLED session
        // the idle reaper closed is provably dead BEFORE the send — the
        // queued FIN answers a non-blocking peek — so replacing it here
        // costs no attempt and touches no retry law (the one-attempt
        // classes refuse only the true sent-then-lost ambiguity). Without
        // this screen, every un-witnessed mutator following a ≥ 60 s
        // quiet spell surfaced EINVAL to the application.
        if guard.as_ref().is_some_and(|c| c.dead_on_arrival()) {
            *guard = None;
        }
        // Rung 18 (residual d — the idle-reap conviction): a TRANSPORT
        // failure on a resend-safe call reconnects and resends the SAME
        // frame once (the S8 batch precedent; classification is
        // STRUCTURAL — a refusal comes back as Ok(reply) with a status,
        // so a call error is always the dead-session class). The live
        // fleet's 60 s idle-session reaper makes a dead first session the
        // NORMAL state of any verb that follows a quiet spell.
        let attempts = if resend_safe { 2 } else { 1 };
        let mut reply = None;
        let mut last_err = None;
        for attempt in 0..attempts {
            if guard.is_none() {
                match RpcClient::connect(endpoint, &self.secret, &self.peer_id, None).await {
                    Ok(c) => *guard = Some(c),
                    Err(e) => {
                        last_err = Some(e);
                        continue;
                    }
                }
            }
            // Rung 9 (S8-a attribution): a publish-vocabulary ship pays the
            // same authenticated round trip as an S8 trait verb, so it records
            // the SAME `meta_ship_phase_ns.rtt` phase — the published serial
            // A/B's rtt column covers the whole shipped stream, not just the
            // trait half.
            let t_rtt = std::time::Instant::now();
            let out = guard
                .as_mut()
                .expect("connected above")
                .call(VERB_PUBLISH_CALL, body.clone())
                .await;
            super::phase_record(super::ShipPhase::Rtt, t_rtt);
            match out {
                Ok(r) => {
                    reply = Some(r);
                    break;
                }
                Err(e) => {
                    *guard = None;
                    if attempt + 1 < attempts {
                        log::warn!(
                            "S9: publish {name} to {endpoint} failed ({e}) — reconnecting and \
                             resending the same frame (resend-safe class: witnessed / monotone \
                             / read; the idle-session reaper makes a dead first session normal)"
                        );
                    }
                    last_err = Some(e);
                }
            }
        }
        let Some(reply) = reply else {
            return Err(last_err.unwrap_or_else(|| {
                SqueezefsError::InvalidOperation(format!(
                    "S9: no session to {endpoint} and no error to report for {name}"
                ))
            }));
        };
        drop(guard);
        SHIPPED.fetch_add(1, Ordering::Relaxed);
        if reply.status == PUBLISH_STALE_LEASE {
            let detail = String::from_utf8_lossy(&reply.body).to_string();
            let fenced = match (presented, crate::data_grant::custody_client()) {
                (Some(epoch), Some(client)) => client.note_publish_era_refused(epoch, &detail),
                _ => false,
            };
            log::error!(
                "S9: the authority at {endpoint} refused {name} BY ERA ({detail}) — nothing \
                 was applied{}",
                if fenced {
                    "; this epoch was our CURRENT lease, so the full fence composed (custody \
                     poisoned — re-admission is by remount)"
                } else {
                    " (the refused epoch is not this mount's current lease — a dead frame, \
                     not a dead era)"
                }
            );
            return Err(SqueezefsError::WriterGuardFenced);
        }
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

/// Rung 17: the OWNER-side per-ino serialization of served layout-class
/// verbs (design §6a law 3's owner half, made structural): the
/// custody-scoped full Put reads the durable layout BEFORE its commit,
/// and two clients' serves of one ino must not interleave in that window
/// (the backend's own 4a guard covers the commit, not the read). Striped;
/// collisions only serialize spuriously.
static SERVE_INO_LOCKS: Lazy<
    crate::stripe_locks::StripeLocks<crate::sqz_sync::SqzMutex<()>, 1024>,
> = Lazy::new(crate::stripe_locks::StripeLocks::new);

/// Rung 18 — the LOCAL half of the serve-window law (the s11-subblock
/// C8/C2 dangling-take mint's second face): a served scoped Put reads
/// the durable head, composes, and commits under `SERVE_INO_LOCKS`; the
/// AUTHORITY's OWN layout publishes of a RANGE-GRANTED ino (the
/// assembler's fold, its writeback) ran outside it, so a fold's merge
/// landing inside a Put's read→commit window was erased by the Put's
/// full map — the fold's freshly-taken key lost its reference while its
/// take stood. A local publish of a range-granted ino therefore takes
/// the SAME stripe (one relaxed probe + one map read on every solo
/// mount: `custody_owner()` is None). The serve wrapper deliberately
/// exempts the EXTENT verbs so their executors' folds can take this
/// guard at the funnel without self-deadlocking.
async fn local_publish_guard(ino: Ino) -> Option<crate::sqz_sync::SqzMutexGuard<'static, ()>> {
    if crate::data_grant::custody_owner().is_some() && crate::dlm::ino_has_range_custody(ino) {
        Some(SERVE_INO_LOCKS.get_inode_lock(ino).lock().await)
    } else {
        None
    }
}

squeezefs_ipc::sqz_task_local! {
    /// Finding 26: the ARBITER-FOLD scope — armed by the authority's
    /// rung-17 executors (`assemble_shipped_extent` /
    /// `flush_shipped_extents`) around the write/fold ladder that
    /// executes SHIPPED-ASSEMBLY bytes. Inside it the range-shared
    /// fast-path clauses ask the arbiter's form of the sharing question
    /// (`dlm::span_range_shared_for_arbiter` — two-or-more distinct
    /// holders) instead of the holder-token form, which read the folded
    /// holder's own grant as foreign and declined the fold's fast paths
    /// on every pass. Task-scoped (the ladder awaits in-task; a spawned
    /// subtask deliberately falls back to the conservative holder form —
    /// the `AUTHORITY_FREE_SCOPE` precedent, same rail: no tokio in the
    /// lib).
    static ARBITER_FOLD: ();
}

/// Run `f` under the arbiter-fold scope (see [`ARBITER_FOLD`]).
pub async fn with_arbiter_fold<F: std::future::Future>(f: F) -> F::Output {
    ARBITER_FOLD.scope((), f).await
}

/// Is the current task inside the arbiter-fold scope?
pub fn arbiter_fold_active() -> bool {
    ARBITER_FOLD.try_with(|_| ()).is_ok()
}

/// Finding 28: the BINDING PROBE — answers "may this block key be
/// adopted into a durable head?" (`false` ⇔ its stamped incarnation is
/// DEAD on this authority). Installed by the mount arm (wired to
/// `BackendRouter::block_key_incarnation_ok`); absent = adopt everything
/// (the pre-f28 shape — solo mounts never serve merges). The law it
/// enforces: a caller's stale map entry never REGRESSES a block the
/// arbiter's fold already displaced — the probe's refusal drops the
/// ENTRY (the durable's stands, the frame still serves), because
/// adopting it wedged every later fold/read of the block into
/// "names a dead incarnation" EIO until the shipper's parked publish
/// could never heal it (the first cheap-first probe's MPI_ABORT).
static BINDING_PROBE: parking_lot::RwLock<Option<Arc<dyn Fn(&str) -> bool + Send + Sync>>> =
    parking_lot::RwLock::new(None);

/// Install the finding-28 binding probe (the mount arm; a re-arm
/// replaces).
pub fn install_binding_probe(probe: Arc<dyn Fn(&str) -> bool + Send + Sync>) {
    *BINDING_PROBE.write() = Some(probe);
}

/// Drop dead-incarnation entries from a caller's map-entry set; returns
/// how many dropped (counted on `publish_stale_binding_drops`).
fn retain_live_bindings(entries: &mut Vec<(u32, String)>, ino: Ino) -> u64 {
    let probe = BINDING_PROBE.read().clone();
    let Some(probe) = probe else { return 0 };
    let before = entries.len();
    entries.retain(|(b, k)| {
        let live = probe(k);
        if !live {
            log::warn!(
                "S9 publish: dropping caller entry block {b} of ino {ino} — key '{k}' \
                 names a DEAD incarnation (finding 28: adopting it would regress the \
                 head past the arbiter's own displacement; the durable entry stands)"
            );
        }
        live
    });
    let dropped = (before - entries.len()) as u64;
    if dropped > 0 {
        crate::fuse_client::METRICS
            .publish_stale_binding_drops
            .fetch_add(dropped, Ordering::Relaxed);
    }
    dropped
}

/// Finding 25 (rung A): the SETTLE arms' serve-window participation —
/// the serialized settle resolve holds (3) + (3.5), but a SERVED publish
/// commits under neither, so a serve landing mid-window could still move
/// the binding under a backend-fresh resolve and burn the tripwire on
/// legal traffic. The settle takes the SAME per-ino serve stripe (order:
/// (3) → (3.5) → serve stripe — consistent with the local-publish order,
/// where the router's save holds (3.5) and `set_layout_and_size` takes
/// the stripe inside). `None` exactly when `local_publish_guard` answers
/// `None` (no custody owner / no range custody): a solo mount pays one
/// relaxed probe.
pub(crate) async fn settle_serve_window(
    ino: Ino,
) -> Option<crate::sqz_sync::SqzMutexGuard<'static, ()>> {
    local_publish_guard(ino).await
}

/// Test seam (the structural pin's venue): the serve stripe for `ino`,
/// so a suite can hold the serve window open and prove a local publish
/// of a range-granted ino PARKS on it.
#[doc(hidden)]
pub async fn test_lock_serve_ino(ino: Ino) -> crate::sqz_sync::SqzMutexGuard<'static, ()> {
    SERVE_INO_LOCKS.get_inode_lock(ino).lock().await
}

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

/// The owner of `ino`'s volume, or `None` when this node owns it — and a
/// loud refusal when the volume's ownership entry is POISONED (§5.10: a
/// re-derivation found a holder the durable assignment set does not name,
/// so neither appending locally nor shipping is an answer this mount may
/// give).
///
/// One relaxed load on an unarmed mount, which is every mount that ships.
#[inline]
fn owner_of(be: &Arc<RoutedMetaBackend>, ino: Ino) -> Result<Option<Arc<super::PeerOwner>>> {
    if !super::ownership_armed() {
        return Ok(None);
    }
    let (v_idx, _) = be.route_ino(ino);
    super::owners::route_volume(v_idx)
}

/// [`owner_of`] for the PREDICATE sites: a poisoned volume reads as "not
/// local", so the merge takes its chain-onto-head arm and the publish that
/// follows is what refuses. A predicate cannot refuse, and inventing a
/// second refusal here would let one poisoned volume answer two ways.
#[inline]
fn owner_of_unchecked(be: &Arc<RoutedMetaBackend>, ino: Ino) -> Option<Arc<super::PeerOwner>> {
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

/// Rung 17: does `ino`'s layout merge take the CHAIN-ONTO-HEAD arm — a
/// foreign-home ino (the merge ships and the OWNER chains it) or an ino
/// this authority granted live foreign custody on (its own RAM
/// provenance goes stale under served shipped merges)? One relaxed-load
/// ladder on every solo mount (`ownership_armed` false, `custody_owner`
/// None).
pub fn merge_is_chained(be: &Arc<RoutedMetaBackend>, ino: Ino) -> bool {
    owner_of_unchecked(be, ino).is_some()
        || crate::data_grant::custody_owner()
            .map(|o| o.ino_granted(ino))
            .unwrap_or(false)
}

/// Rung 13 — the publish-path ORDERING BARRIER: a shipped publish naming
/// a PENDING intent ino (a locally-minted, not-yet-applied create) must
/// flush the intent batch first, or the publish would reach the owner
/// before the record it names exists. One relaxed load when nothing is
/// pending.
async fn intent_barrier_inos(inos: &[Ino]) -> Result<()> {
    if !super::intents::barrier_needed(inos, &[]) {
        return Ok(());
    }
    super::intents::flush_all(true).await.map_err(|errno| {
        SqueezefsError::refused(
            errno,
            "S10 intents: the publish barrier's flush failed — refusing the publish rather \
             than letting it overtake the un-applied create it names"
                .to_string(),
        )
    })
}

/// The custody lease epoch this mount presents on every MUTATING shipped
/// publish (finding #6's era gate). `0` with no custody client installed —
/// the owner then refuses by era, which is the honest shape for an armed
/// ownership plane missing its custody half (0 is never a live epoch).
fn current_lease_epoch() -> u64 {
    crate::data_grant::custody_client()
        .map(|c| c.lease_epoch())
        .unwrap_or(0)
}

/// Bounded resend budget for one witnessed layout publish — the
/// [`crate::cowriter`] `FREE_SHIP_ATTEMPTS` law, verbatim: a protocol
/// constant, not a resource cap; each resend is absorbed exactly-once by
/// the owner's witness window, and past the budget the error propagates to
/// the writeback ladder, which re-COMPUTES a new logical publish from
/// current state (the module's convergence law).
const PUBLISH_SHIP_ATTEMPTS: u32 = 3;

/// Ship one WITNESSED layout-publish call (design-mw-layout-versions §6a
/// law 2): the SAME frame — same `(lease_epoch, request_id)`, same payload
/// — under a bounded, epoch-stable retry ladder. **A retry never
/// re-keys**: if the lease epoch moves under it (revocation → re-join) the
/// publish is abandoned to the caller, because a resend under a new epoch
/// is a new act the window cannot correlate. A fence-class refusal
/// (`PUBLISH_STALE_LEASE` → `WriterGuardFenced`) never retries — the era
/// is dead and every resend would refuse identically.
async fn ship_witnessed(peer: &Arc<super::PeerOwner>, call: PublishCall) -> Result<PublishReply> {
    let epoch = call
        .presented_epoch()
        .expect("only witnessed (epoch-bearing) calls ride this ladder");
    let mut attempt = 0u32;
    loop {
        match ship(peer, call.clone()).await {
            Ok(reply) => return Ok(reply),
            Err(e @ SqueezefsError::WriterGuardFenced) => return Err(e),
            Err(e) => {
                attempt += 1;
                if current_lease_epoch() != epoch || attempt >= PUBLISH_SHIP_ATTEMPTS {
                    return Err(e);
                }
                squeezefs_ipc::sqz_time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
    }
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
    match owner_of(be, ino)? {
        None => {
            note_local();
            let _serve_window = local_publish_guard(ino).await;
            be.set_layout_and_size(ino, layout, size, refs).await
        }
        Some(peer) => {
            intent_barrier_inos(&[ino]).await?;
            expect_unit(
                ship_witnessed(
                    &peer,
                    PublishCall::SetLayoutAndSize {
                        ino,
                        layout: layout.to_vec(),
                        size,
                        refs: wire_refs(refs),
                        lease_epoch: current_lease_epoch(),
                        request_id: crate::cowriter::next_ship_request_id(),
                    },
                )
                .await?,
                "set_layout_and_size",
            )
        }
    }
}

/// Routed [`RoutedMetaBackend::merge_layout_and_size`]. Returns
/// `(use_delta, staged_version)` — the staged link's version (0 on a
/// full-Put commit), which the caller stamps into the RAM provenance so
/// the next delta claims the right base (rung 17's chain-without-refetch
/// law; on the un-chained local arm the version is the delta's own).
pub async fn merge_layout_and_size(
    be: &Arc<RoutedMetaBackend>,
    ino: Ino,
    delta: &crate::layout_wire::LayoutDelta,
    full_layout: bytes::Bytes,
    size: u64,
    refs: Vec<BlockRefOp>,
) -> Result<(bool, u64)> {
    match owner_of(be, ino)? {
        None => {
            note_local();
            // Rung 17: the AUTHORITY's own publishes on an ino with live
            // foreign custody must ALSO chain onto the head — its RAM
            // provenance goes stale under every served shipped merge, and
            // the un-chained gate would refuse (then full-Put-clobber the
            // peers' blocks). One relaxed probe on every solo mount
            // (`custody_owner()` is None).
            let granted = crate::data_grant::custody_owner()
                .map(|o| o.ino_granted(ino))
                .unwrap_or(false);
            let _serve_window = local_publish_guard(ino).await;
            if granted {
                be.merge_layout_and_size_chained(ino, delta, full_layout, size, refs)
                    .await
            } else {
                let used = be
                    .merge_layout_and_size(ino, delta, full_layout, size, refs)
                    .await?;
                Ok((used, if used { delta.version } else { 0 }))
            }
        }
        Some(peer) => {
            intent_barrier_inos(&[ino]).await?;
            let call = PublishCall::MergeLayoutAndSize {
                ino,
                delta: delta.encode(),
                full_layout: full_layout.to_vec(),
                size,
                refs: wire_refs(&refs),
                lease_epoch: current_lease_epoch(),
                request_id: crate::cowriter::next_ship_request_id(),
            };
            match ship_witnessed(&peer, call).await? {
                PublishReply::DeltaUsed { used, version } => Ok((used, version)),
                other => Err(protocol_error(
                    "merge_layout_and_size",
                    &format!("{other:?}"),
                    "a delta-used flag",
                )),
            }
        }
    }
}

/// **Ship one sub-block extent of a SHARED block to its authority**
/// (rung 17, KD-MW-8) and return the ack's `covering_version` (`Some`
/// iff the covering publish already ran — the shipper's retention
/// releases at the round trip). Rides the witnessed same-frame bounded
/// epoch-stable resend ladder; a local-home ino REFUSES loud (the
/// authority never ships extents to itself — its writes ARE the
/// assembly).
pub async fn write_extent(
    be: &Arc<RoutedMetaBackend>,
    ino: Ino,
    block_index: u64,
    offset_in_block: u32,
    data: Vec<u8>,
    token: u64,
    request_id: u64,
) -> Result<Option<u64>> {
    match owner_of(be, ino)? {
        None => Err(SqueezefsError::InvalidOperation(format!(
            "S11: write_extent for ino {ino} routes LOCALLY — the authority assembles its \
             own writes through its write path, never through the extent wire (a local \
             extent ship is a routing bug worth a loud refusal)"
        ))),
        Some(peer) => {
            intent_barrier_inos(&[ino]).await?;
            let call = PublishCall::WriteExtent {
                ino,
                block_index,
                offset_in_block,
                data,
                token,
                lease_epoch: current_lease_epoch(),
                request_id,
            };
            match ship_witnessed(&peer, call).await? {
                PublishReply::ExtentAck { covering_version } => {
                    EXTENT_SHIPPED.fetch_add(1, Ordering::Relaxed);
                    Ok(covering_version)
                }
                other => Err(protocol_error(
                    "write_extent",
                    &format!("{other:?}"),
                    "an extent ack",
                )),
            }
        }
    }
}

/// **The fsync force** (rung 17, §9.3's retention law): fold + publish
/// every extent the authority holds for `ino`, returning the covering
/// layout version. Witnessed and retried (idempotent).
pub async fn flush_extents(be: &Arc<RoutedMetaBackend>, ino: Ino) -> Result<u64> {
    match owner_of(be, ino)? {
        None => Err(SqueezefsError::InvalidOperation(format!(
            "S11: flush_extents for ino {ino} routes LOCALLY — nothing was ever shipped \
             (the authority's own fsync is its ordinary flush path)"
        ))),
        Some(peer) => {
            let call = PublishCall::FlushExtents {
                ino,
                lease_epoch: current_lease_epoch(),
                request_id: crate::cowriter::next_ship_request_id(),
            };
            match ship_witnessed(&peer, call).await? {
                PublishReply::FlushDone { covering_version } => {
                    EXTENT_FLUSH_FORCES.fetch_add(1, Ordering::Relaxed);
                    Ok(covering_version)
                }
                other => Err(protocol_error(
                    "flush_extents",
                    &format!("{other:?}"),
                    "a covering version",
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
    match owner_of(be, ino)? {
        None => {
            note_local();
            be.commit_block_refs(ino, refs).await
        }
        Some(peer) => {
            intent_barrier_inos(&[ino]).await?;
            expect_unit(
                ship_witnessed(
                    &peer,
                    PublishCall::CommitBlockRefs {
                        ino,
                        refs: wire_refs(refs),
                        lease_epoch: current_lease_epoch(),
                        request_id: crate::cowriter::next_ship_request_id(),
                    },
                )
                .await?,
                "commit_block_refs",
            )
        }
    }
}

/// Routed [`RoutedMetaBackend::park_write_times`].
pub async fn park_write_times(
    be: &Arc<RoutedMetaBackend>,
    ino: Ino,
    mtime: u64,
    ctime: u64,
) -> Result<()> {
    match owner_of(be, ino)? {
        None => {
            note_local();
            be.park_write_times(ino, mtime, ctime).await
        }
        Some(peer) => {
            intent_barrier_inos(&[ino]).await?;
            expect_unit(
                ship(
                    &peer,
                    PublishCall::ParkWriteTimes {
                        ino,
                        mtime,
                        ctime,
                        lease_epoch: current_lease_epoch(),
                    },
                )
                .await?,
                "park_write_times",
            )
        }
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
        match owner_of(be, ino)? {
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
        intent_barrier_inos(&batch).await?;
        expect_unit(
            ship(
                &peer,
                PublishCall::DestroyInodes {
                    inos: batch,
                    lease_epoch: current_lease_epoch(),
                },
            )
            .await?,
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
    match owner_of(be, parent)? {
        None => {
            note_local();
            be.create_with_rdev_size(parent, name, mode, uid, gid, rdev, initial_size)
                .await
        }
        Some(peer) => {
            // Rung 13: symlinks/sized creates mint locally too under a
            // live UPDATE grant (the target payload's own publish then
            // barriers on the pending ino, forcing the flush).
            match super::intents::try_mint_create(
                &peer.endpoint,
                parent,
                name,
                mode,
                uid,
                gid,
                rdev,
                initial_size,
            ) {
                super::intents::MintOutcome::Minted(inode) => return Ok(inode),
                super::intents::MintOutcome::Exists => {
                    return Err(SqueezefsError::already_exists("File already exists"))
                }
                super::intents::MintOutcome::NotEligible => {}
            }
            intent_barrier_inos(&[parent]).await?;
            let call = PublishCall::CreateWithRdevSize {
                parent,
                name: name.to_string(),
                mode,
                uid,
                gid,
                rdev,
                initial_size,
                lease_epoch: current_lease_epoch(),
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
    match owner_of(be, ino)? {
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
    match owner_of(be, 1)? {
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

// ---------------------------------------------------------------------------
// DLM S9 — the co-writer FREE path (contracts tests/mw_cowriter_free_tests.rs;
// operator story docs/operations.md §Multi-writer co-writer mounts). Kept in
// its own block like the lane raise above: the free verb is the SECOND
// metadata-plane act a co-writer's data path performs, and — unlike every
// other call here — it is RETRIED, which only the dedup window makes safe.
// ---------------------------------------------------------------------------

/// The owner-side FREE executor: `(vol_tag, block indices)` → one verdict
/// per block, produced by running the AUTHORITY's own free ladder
/// ([`crate::cowriter::execute_shipped_frees`] over its data-plane router).
///
/// Installed by the multi-writer AUTHORITY arm beside the frontier source
/// (`crate::multi_writer::arm_multi_writer`); a served free with no
/// executor refuses loud — the ownership plane armed without its free half
/// is the same class as the missing-publish-client refusal below.
pub type FreeExecutor = Arc<
    dyn Fn(u64, Vec<u64>) -> Pin<Box<dyn Future<Output = Result<Vec<FreeVerdict>>> + Send>>
        + Send
        + Sync,
>;

static FREE_EXECUTOR: Lazy<arc_swap::ArcSwapOption<FreeExecutor>> =
    Lazy::new(arc_swap::ArcSwapOption::empty);

/// Install the process's shipped-free executor (the authority arm's act).
pub fn install_free_executor(exec: FreeExecutor) {
    FREE_EXECUTOR.store(Some(Arc::new(exec)));
}

/// Uninstall it (disarm / unmount / test teardown).
pub fn uninstall_free_executor() {
    FREE_EXECUTOR.store(None);
}

fn free_executor() -> Option<FreeExecutor> {
    FREE_EXECUTOR.load_full().map(|e| (*e).clone())
}

/// The owner-side lane-free HARVEST executor (rung 10, residual 2):
/// `(vol_tag, lane, writers, max, lease_epoch)` → the handed-out block
/// indices ([`crate::cowriter::execute_lane_harvest`] over the authority's
/// data-plane router).
///
/// Installed by the multi-writer AUTHORITY arm beside the free executor —
/// the two are halves of one rewrite economy: the free RETURNS a co-writer's
/// displaced offset to the lane's supply, the harvest is what makes that
/// supply REACHABLE again.
pub type HarvestExecutor = Arc<
    dyn Fn(u64, u16, u16, u64, u64) -> Pin<Box<dyn Future<Output = Result<Vec<u64>>> + Send>>
        + Send
        + Sync,
>;

static HARVEST_EXECUTOR: Lazy<arc_swap::ArcSwapOption<HarvestExecutor>> =
    Lazy::new(arc_swap::ArcSwapOption::empty);

/// Install the process's lane-harvest executor (the authority arm's act).
pub fn install_harvest_executor(exec: HarvestExecutor) {
    HARVEST_EXECUTOR.store(Some(Arc::new(exec)));
}

/// Uninstall it (disarm / unmount / test teardown).
pub fn uninstall_harvest_executor() {
    HARVEST_EXECUTOR.store(None);
}

fn harvest_executor() -> Option<HarvestExecutor> {
    HARVEST_EXECUTOR.load_full().map(|e| (*e).clone())
}

/// One shipped extent as the owner-side assembler executor receives it
/// (rung 17): the wire frame's fields plus the shipper's identity (the
/// coverage ledger's key).
#[derive(Debug, Clone)]
pub struct ExtentFrame {
    pub client: String,
    pub ino: u64,
    pub block_index: u64,
    pub offset_in_block: u32,
    pub data: Vec<u8>,
    pub token: u64,
    pub lease_epoch: u64,
    pub request_id: u64,
}

/// The owner-side extent MERGE executor (rung 17): merge one shipped
/// extent into the authority's assembly for `(ino, block_index)` —
/// production: the fs's extent overlay under `BLOCK_FLUSH_LOCKS`, with
/// every assembly DMA authorized under the AUTHORITY'S OWN epoch
/// (`data_custody::authorize_dma(None)` — never as an exercise of a
/// shipper's grant; the §9.3 custody transfer). Returns the covering
/// version when the extent is ALREADY covered by a durable publish
/// (`Some` releases the shipper's retention at the ack), else `None`.
pub type ExtentMergeExec = Arc<
    dyn Fn(ExtentFrame) -> Pin<Box<dyn Future<Output = Result<Option<u64>>> + Send>> + Send + Sync,
>;

static EXTENT_MERGE_EXEC: Lazy<arc_swap::ArcSwapOption<ExtentMergeExec>> =
    Lazy::new(arc_swap::ArcSwapOption::empty);

/// Install the assembler's merge executor (the authority arm's act).
pub fn install_extent_merge_executor(exec: ExtentMergeExec) {
    EXTENT_MERGE_EXEC.store(Some(Arc::new(exec)));
}

/// Uninstall it (disarm / unmount / test teardown).
pub fn uninstall_extent_merge_executor() {
    EXTENT_MERGE_EXEC.store(None);
}

fn extent_merge_executor() -> Option<ExtentMergeExec> {
    EXTENT_MERGE_EXEC.load_full().map(|e| (*e).clone())
}

/// The owner-side extent FLUSH executor (rung 17's fsync force): fold and
/// publish every assembled extent for `ino`, answer the covering layout
/// version. Production advances the coverage ledger
/// ([`crate::extent_ship::owner_note_covered`]) before answering.
pub type ExtentFlushExec =
    Arc<dyn Fn(u64) -> Pin<Box<dyn Future<Output = Result<u64>> + Send>> + Send + Sync>;

static EXTENT_FLUSH_EXEC: Lazy<arc_swap::ArcSwapOption<ExtentFlushExec>> =
    Lazy::new(arc_swap::ArcSwapOption::empty);

/// Install the assembler's flush executor (the authority arm's act).
pub fn install_extent_flush_executor(exec: ExtentFlushExec) {
    EXTENT_FLUSH_EXEC.store(Some(Arc::new(exec)));
}

/// Uninstall it (disarm / unmount / test teardown).
pub fn uninstall_extent_flush_executor() {
    EXTENT_FLUSH_EXEC.store(None);
}

/// Rung 18 — the AUTHORITY-side coherence sink (the s11-subblock
/// dangling-take mint's THIRD face): a SERVED layout-class publish
/// commits on the backend DIRECTLY, bypassing the authority's own
/// fs-level RAM metadata cache — so its fold/writeback later computed a
/// DISPLACED-release set from a view that never saw the co-writer's last
/// direct merge, and the superseded key's take dangled ([C8] '1 durable
/// vs 0 layout references' + [C2] leak). The mount arm installs the
/// authority fs's invalidation here (`metadata_cache.remove` + attr
/// invalidate — the co-writer side has carried the mirror-image
/// `install_release_hook` since rung 17); the serve wrapper calls it for
/// every successfully committed layout-MUTATING verb's named inos.
pub type ServedLayoutInval = Arc<dyn Fn(u64) + Send + Sync>;

static SERVED_LAYOUT_INVAL: Lazy<arc_swap::ArcSwapOption<ServedLayoutInval>> =
    Lazy::new(arc_swap::ArcSwapOption::empty);

pub fn install_served_layout_invalidation(sink: ServedLayoutInval) {
    SERVED_LAYOUT_INVAL.store(Some(Arc::new(sink)));
}

pub fn uninstall_served_layout_invalidation() {
    SERVED_LAYOUT_INVAL.store(None);
}

fn note_served_layout_commit(inos: &[u64]) {
    if let Some(sink) = SERVED_LAYOUT_INVAL.load_full() {
        for ino in inos {
            sink(*ino);
        }
    }
}

fn extent_flush_executor() -> Option<ExtentFlushExec> {
    EXTENT_FLUSH_EXEC.load_full().map(|e| (*e).clone())
}

/// **Ship one lane-free harvest** to the authority at `endpoint` and return
/// the granted block indices.
///
/// The idempotence witness travels verbatim, exactly as
/// [`ship_free_blocks`]'s does: `(lease_epoch, request_id)` is the owner's
/// dedup key — a resend after a lost reply answers the winner's own grant,
/// and a retry never re-keys across a re-join.
/// Returns `(handed-out block indices, the authority's bound-age hint in
/// ms)` — the hint is schema 8's OQ 2 field (0 = the authority's ring
/// holds nothing; the caller falls back to its derivation).
pub async fn ship_harvest_lane_free(
    endpoint: &str,
    vol_tag: u64,
    lane: u16,
    writers: u16,
    max: u64,
    lease_epoch: u64,
    request_id: u64,
) -> Result<(Vec<u64>, u64)> {
    let Some(client) = CLIENT.load_full() else {
        REFUSALS.fetch_add(1, Ordering::Relaxed);
        let msg = format!(
            "S9: a lane free harvest for vol_tag {vol_tag:#016x} cannot be published — no \
             publish client is installed (arm the co-writer mount, which installs both halves)"
        );
        log::error!("{msg}");
        return Err(SqueezefsError::InvalidOperation(msg));
    };
    let call = PublishCall::HarvestLaneFree {
        vol_tag,
        lane,
        writers,
        max,
        lease_epoch,
        request_id,
    };
    match client.ship(endpoint, call).await? {
        PublishReply::LaneFreeGrant {
            blocks,
            bound_age_ms,
        } => {
            HARVEST_SHIPPED_BLOCKS.fetch_add(blocks.len() as u64, Ordering::Relaxed);
            Ok((blocks, bound_age_ms))
        }
        other => Err(protocol_error(
            "harvest_lane_free",
            &format!("{other:?}"),
            "a lane free grant",
        )),
    }
}

/// Count `blocks` abandoned shipped frees (`free_ship_failures` — the
/// leak-safe direction, loud). The one caller is
/// [`crate::cowriter::ship_displaced_frees`]'s abandon arm.
pub(crate) fn note_free_ship_failure(blocks: u64) {
    FREE_SHIP_FAILURES.fetch_add(blocks, Ordering::Relaxed);
}

/// **Ship one displaced-free verb** to the authority at `endpoint` and
/// return its per-block verdicts.
///
/// The idempotence witness travels verbatim: `(lease_epoch, request_id)` is
/// the owner's dedup key, so a caller MAY resend this exact call after a
/// lost reply — and MUST NOT re-key it (a fresh id, or the same id under a
/// fresh epoch, is a new act; the retry ladder in
/// [`crate::cowriter::ship_displaced_frees`] is the one sanctioned caller).
pub async fn ship_free_blocks(
    endpoint: &str,
    vol_tag: u64,
    blocks: Vec<u64>,
    lease_epoch: u64,
    request_id: u64,
) -> Result<Vec<FreeVerdict>> {
    let count = blocks.len() as u64;
    let Some(client) = CLIENT.load_full() else {
        REFUSALS.fetch_add(1, Ordering::Relaxed);
        let msg = format!(
            "S9: a displaced-block free for vol_tag {vol_tag:#016x} cannot be published — no \
             publish client is installed. Freeing locally would mutate ownership accounting \
             whose durable home is the authority's ledger, so this refuses instead (arm the \
             co-writer mount, which installs both halves)"
        );
        log::error!("{msg}");
        return Err(SqueezefsError::InvalidOperation(msg));
    };
    let call = PublishCall::FreeBlocks {
        vol_tag,
        blocks,
        lease_epoch,
        request_id,
    };
    match client.ship(endpoint, call).await? {
        PublishReply::FreeVerdicts(verdicts) => {
            FREE_SHIPPED_BLOCKS.fetch_add(count, Ordering::Relaxed);
            let refused = verdicts
                .iter()
                .filter(|v| **v == FreeVerdict::Refused)
                .count();
            if refused > 0 {
                log::error!(
                    "S9: the authority refused {refused} shipped free(s) (vol_tag \
                     {vol_tag:#016x}, request {request_id}): already free/graced/quarantined — \
                     the double-release lineage, surfaced on the authority's own tripwire"
                );
            }
            Ok(verdicts)
        }
        other => Err(protocol_error(
            "free_blocks",
            &format!("{other:?}"),
            "per-block free verdicts",
        )),
    }
}

/// **Ship one block-ref population read** to the owner at `endpoint` —
/// the owner-partitioned half of [`crate::cowriter::durable_block_refcounts`]
/// (finding 13): the answer is the peer's LIVE-tree count over the volumes
/// it owns, in request order.
///
/// Pure read, transport-resend-safe; a failure is returned loud — the
/// shipped-free serve that needed it refuses rather than validating
/// against a lagged local snapshot (the caller's retry ladder owns the
/// leak-safe abandon).
pub async fn ship_block_ref_population(
    endpoint: &str,
    vol_tag: u64,
    block_idxs: Vec<u64>,
) -> Result<Vec<u64>> {
    let expected = block_idxs.len();
    let Some(client) = CLIENT.load_full() else {
        return Err(SqueezefsError::InvalidOperation(format!(
            "S9: a block-reference population read for vol_tag {vol_tag:#016x} cannot be \
             shipped — no publish client is installed. Answering from the local lagged \
             snapshot instead would validate a free against state the volume's owner has \
             already moved (finding 13), so this refuses"
        )));
    };
    let call = PublishCall::BlockRefPopulation {
        vol_tag,
        block_idxs,
    };
    match client.ship(endpoint, call).await? {
        PublishReply::Populations(counts) if counts.len() == expected => Ok(counts),
        PublishReply::Populations(counts) => Err(SqueezefsError::InvalidOperation(format!(
            "S9: the owner at {endpoint} answered {} population(s) for {expected} block \
             index(es) — refusing a reply that cannot be paired with its request",
            counts.len()
        ))),
        other => Err(protocol_error(
            "block_ref_population",
            &format!("{other:?}"),
            "per-index reference populations",
        )),
    }
}

/// Routed [`RoutedMetaBackend::readdir_stream`].
pub async fn readdir_stream(
    be: &Arc<RoutedMetaBackend>,
    dir: Ino,
    offset: u64,
    max: usize,
) -> Result<Vec<(u64, DirEntry)>> {
    match owner_of(be, dir)? {
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

/// Rung 20 residual 1: the custody-scoped compose's blob custody, carried
/// from [`PublishService::custody_scoped_layout`] back to the `execute()`
/// arm — `Default` (all-inert) on every verbatim/inline arm.
#[derive(Default)]
struct ScopedBlobCustody {
    /// RES-9 guard for a freshly written re-spill blob: disarmed after
    /// `set_layout_and_size` lands; dropped ARMED on any error path
    /// (freeing the fresh blob).
    fresh: Option<crate::meta_backend::kv::indirect_map::IndirectBlobGuard>,
    /// The compose arm recomputes blob custody ENTIRELY — the caller's
    /// `is_map_blob()` frame ops must NOT extend the recomputed refs.
    drop_caller_blob_ops: bool,
    /// Displaced blob keys (the durable side's old blob) freed strictly
    /// AFTER the commit stopped naming them.
    free_after_commit: Vec<String>,
}

/// The publish path executed for a peer, against the volumes this node has
/// authority over.
///
/// The **venue rule** is S8's, verbatim and for the same mechanical reason:
/// the frame arrives on a pinned `sqz-cluster-svc{n}` lane, and the
/// execution is handed to the **sqz-meta pool**
/// ([`crate::meta_exec::spawn_meta_join`]) — the venue that owns the
/// backend's tasks, because `commit_tx` spawns the per-volume conveyor pass
/// task there, and a verb executed inline on a lane would give a volume's
/// whole commit conveyor a venue whose lifetime is the lane's.
pub struct PublishService {
    inner: Arc<RoutedMetaBackend>,
    authority: Vec<bool>,
    /// The FREE verb's exactly-once witness: `(lease_epoch, request_id)` →
    /// the winner's own outcome — S8's [`DedupWindow`], reused (never a
    /// third idempotence pattern).
    free_dedup: DedupWindow<std::result::Result<Vec<FreeVerdict>, WireError>>,
    /// The LAYOUT-PUBLISH class's exactly-once witness (finding #6, design
    /// §6a law 2): `SetLayoutAndSize` / `MergeLayoutAndSize` /
    /// `CommitBlockRefs` joined the retried class, and a freeze-window
    /// lost-reply re-ship answers the winner's own cached outcome here
    /// instead of re-applying (the divergent-chain mint, closed). Its own
    /// window — outcomes are [`PublishReply`]s, not free verdicts.
    publish_dedup: DedupWindow<std::result::Result<PublishReply, WireError>>,
    /// The HARVEST verb's exactly-once witness — the same pattern, its own
    /// window (a grant and a verdict list are different outcomes; sharing
    /// one window would make their id spaces collide).
    harvest_dedup: DedupWindow<std::result::Result<Vec<u64>, WireError>>,
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
    pub fn new(inner: Arc<RoutedMetaBackend>) -> Arc<Self> {
        let all: Vec<usize> = (0..inner.volumes.len()).collect();
        Self::with_authority(inner, &all)
    }

    /// [`Self::new`] with an explicit authority set — the shape a node that
    /// owns a SUBSET of the set has.
    pub fn with_authority(inner: Arc<RoutedMetaBackend>, volumes: &[usize]) -> Arc<Self> {
        let mut authority = vec![false; inner.volumes.len()];
        for &v in volumes {
            if let Some(slot) = authority.get_mut(v) {
                *slot = true;
            }
        }
        let me = Arc::new(Self {
            inner,
            authority,
            free_dedup: DedupWindow::new(crate::meta_ship::service::dedup_cap()),
            harvest_dedup: DedupWindow::new(crate::meta_ship::service::dedup_cap()),
            publish_dedup: DedupWindow::new(crate::meta_ship::service::dedup_cap()),
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
        // Finding #6 (design-mw-layout-versions §6a, law 1): the ERA GATE on
        // every schema-5 mutating verb — a swept-but-not-yet-self-fenced
        // zombie's publishes REFUSE here, and a refusal means NOTHING was
        // applied. Refused BEFORE the witness window on purpose (a dead
        // era's replay must never be answered from cache — the FreeBlocks
        // precedent verbatim). The raise/free/harvest verbs keep their own,
        // older gates below (landed counter surface).
        if let Some(epoch) = frame.call.era_gated_epoch() {
            if let Err(reason) = crate::data_grant::validate_publish_era(&frame.client, epoch) {
                // Rung 17: the extent class's era refusals land on their
                // OWN row (`extent_stale_refusals`); the layout class
                // keeps the finding-#6 row.
                if frame.call.is_extent() {
                    EXTENT_STALE_REFUSALS.fetch_add(1, Ordering::Relaxed);
                } else {
                    STALE_REFUSALS.fetch_add(1, Ordering::Relaxed);
                }
                return Self::refuse(req.id, PUBLISH_STALE_LEASE, reason);
            }
        }
        // The layout-publish class (law 2) and rung 17's extent class:
        // witnessed — served through the dedup window, never through the
        // generic dispatch below, whose no-retry law it would otherwise
        // weaken.
        if let Some((epoch, request_id)) = frame.call.witness() {
            return self
                .serve_layout_publish(req.id, epoch, request_id, frame.client, frame.call)
                .await;
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
        // The lane free HARVEST (rung 10, residual 2): validated exactly as
        // the raise is — the lane against the assignment THIS authority
        // made, under the lease epoch it minted — then served through its
        // own dedup window (it is retried, like the free and only like it).
        if let PublishCall::HarvestLaneFree {
            lane,
            writers,
            lease_epoch,
            request_id,
            ..
        } = &frame.call
        {
            if let Err(reason) =
                crate::data_grant::validate_lane_raise(&frame.client, *lease_epoch, *lane, *writers)
            {
                HARVEST_REFUSALS.fetch_add(1, Ordering::Relaxed);
                return Self::refuse(req.id, PUBLISH_LANE_REFUSED, reason);
            }
            return self
                .serve_harvest(req.id, *lease_epoch, *request_id, frame.call)
                .await;
        }
        // The co-writer FREE path: retried (like the harvest above and only
        // it), served through the era gate and then the dedup window —
        // never through the generic dispatch below, whose no-retry law it
        // would otherwise weaken.
        if let PublishCall::FreeBlocks {
            lease_epoch,
            request_id,
            ..
        } = &frame.call
        {
            // The era gate FIRST, before the window: a dead era must be
            // refused whether or not its id once executed — answering a
            // dead era's replay from cache would tell a fenced mount its
            // custody still speaks.
            if let Err(reason) = crate::data_grant::validate_free(&frame.client, *lease_epoch) {
                FREE_STALE_REFUSALS.fetch_add(1, Ordering::Relaxed);
                return Self::refuse(req.id, PUBLISH_STALE_LEASE, reason);
            }
            return self
                .serve_free(req.id, *lease_epoch, *request_id, frame.call)
                .await;
        }
        let Some(me) = self.owned() else {
            return Self::refuse(
                req.id,
                PUBLISH_MALFORMED,
                "S9 publish service is shutting down — no handle to dispatch on".into(),
            );
        };
        let name = frame.call.name();
        let client = frame.client;
        let call = frame.call;
        let joined = crate::meta_exec::spawn_meta_join("meta_ship_publish_verb", async move {
            me.execute(&client, call).await
        })
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

    /// Serve one LAYOUT-PUBLISH call (`SetLayoutAndSize` /
    /// `MergeLayoutAndSize` / `CommitBlockRefs`) through the witness
    /// window (finding #6, design §6a law 2): the winner of
    /// `(lease_epoch, request_id)` executes on the sqz-meta pool; every
    /// duplicate — a freeze-window lost-reply retry, or an overlapping
    /// resend — awaits the winner's own outcome and is counted
    /// (`replays`). The era gate already ran in [`Self::serve`], so a
    /// dead era can never reach (or be answered from) this window.
    async fn serve_layout_publish(
        &self,
        req_id: u64,
        lease_epoch: u64,
        request_id: u64,
        client: String,
        call: PublishCall,
    ) -> RpcResponse {
        let Some(me) = self.owned() else {
            return Self::refuse(
                req_id,
                PUBLISH_MALFORMED,
                "S9 publish service is shutting down — no handle to dispatch on".into(),
            );
        };
        let name = call.name();
        let is_extent = call.is_extent();
        let (slot, owns) = self.publish_dedup.slot((lease_epoch, request_id));
        if !owns {
            if is_extent {
                EXTENT_REPLAYS.fetch_add(1, Ordering::Relaxed);
            } else {
                REPLAYS.fetch_add(1, Ordering::Relaxed);
            }
        }
        let outcome = slot
            .get_or_init(|| async move {
                // The venue rule, verbatim (see serve/serve_free): the
                // commit runs on the sqz-meta pool, never inline on a
                // `sqz-cluster-svc{n}` lane.
                match crate::meta_exec::spawn_meta_join("meta_ship_publish_verb", async move {
                    // Rung 17: per-ino serialization across the whole
                    // serve (the scoped Put's read + commit — see
                    // `SERVE_INO_LOCKS`). Rung 18: the EXTENT verbs are
                    // exempt HERE — their executors run the fs's own
                    // write path, whose local layout publishes now take
                    // this very stripe at the funnel
                    // (`local_publish_guard`), so holding it across the
                    // executor would self-deadlock the FlushExtents fold;
                    // extent merges carry their own per-block stripe
                    // discipline.
                    let ino = call.named_inos().first().copied().unwrap_or(0);
                    let _ino_guard = if call.is_extent() {
                        None
                    } else {
                        Some(SERVE_INO_LOCKS.get_inode_lock(ino).lock().await)
                    };
                    // Rung 18: a committed layout-class serve invalidates
                    // the authority fs's RAM view of the named inos (the
                    // served commit bypassed it — see the sink's doc).
                    let inval_inos = call
                        .serves_mutate_layout()
                        .then(|| call.named_inos())
                        .unwrap_or_default();
                    let out = me.execute(&client, call).await;
                    if out.is_ok() {
                        note_served_layout_commit(&inval_inos);
                    }
                    out
                })
                .await
                {
                    Ok(out) => out.map_err(|e| WireError::from_error(&e)),
                    Err(e) => {
                        // RES-7/RES-8: the unwind is recorded — and CACHED,
                        // so a replay answers the same loud failure instead
                        // of re-running half a commit.
                        PANICS.fetch_add(1, Ordering::Relaxed);
                        log::error!("S9 publish owner-side execution of {name} unwound: {e}");
                        Err(WireError::from_error(&SqueezefsError::InvalidOperation(
                            format!("S9 publish owner-side execution panicked: {e}"),
                        )))
                    }
                }
            })
            .await
            .clone();
        SERVED.fetch_add(1, Ordering::Relaxed);
        // `shipped ≡ served` is the engagement law: served counts the
        // WINNER's execution only (a replay is its own row).
        if is_extent && owns && outcome.is_ok() {
            EXTENT_SERVED.fetch_add(1, Ordering::Relaxed);
        }
        let frame = PublishReplyFrame {
            schema: PUBLISH_SCHEMA,
            outcome,
        };
        match encode(&frame, name) {
            Ok(body) => RpcResponse {
                id: req_id,
                status: PUBLISH_OK,
                body,
            },
            Err(e) => Self::refuse(req_id, PUBLISH_MALFORMED, format!("reply encode: {e}")),
        }
    }

    /// Serve one [`PublishCall::FreeBlocks`] through the dedup window: the
    /// winner of `(lease_epoch, request_id)` executes on the sqz-meta
    /// pool under the installed [`FreeExecutor`]; every duplicate —
    /// a lost-reply retry, or an overlapping resend — awaits the winner's
    /// own outcome and is counted (`free_replays`). The era gate already
    /// ran in [`Self::serve`].
    async fn serve_free(
        &self,
        req_id: u64,
        lease_epoch: u64,
        request_id: u64,
        call: PublishCall,
    ) -> RpcResponse {
        let PublishCall::FreeBlocks {
            vol_tag, blocks, ..
        } = call
        else {
            return Self::refuse(
                req_id,
                PUBLISH_MALFORMED,
                "serve_free dispatched a non-free call".into(),
            );
        };
        let Some(exec) = free_executor() else {
            return Self::refuse(
                req_id,
                PUBLISH_MALFORMED,
                "S9: a shipped free arrived but no free executor is installed — the ownership \
                 plane is armed without its FREE half. Executing it against the metadata set \
                 alone would strand the device reclaim and the free list; arm the multi-writer \
                 authority (which installs the executor beside the frontier source)"
                    .to_string(),
            );
        };
        let (slot, owns) = self.free_dedup.slot((lease_epoch, request_id));
        if !owns {
            FREE_REPLAYS.fetch_add(1, Ordering::Relaxed);
        }
        let outcome = slot
            .get_or_init(|| async move {
                // The venue rule, verbatim: the ladder runs on the
                // sqz-meta pool — the venue that owns the backend's tasks
                // (the reclaim queue's worker and the conveyor live
                // there), never inline on a `sqz-cluster-svc{n}` lane.
                match crate::meta_exec::spawn_meta_join(
                    "shipped_free_ladder",
                    exec(vol_tag, blocks),
                )
                .await
                {
                    Ok(Ok(verdicts)) => {
                        FREE_SERVED_BLOCKS.fetch_add(
                            verdicts
                                .iter()
                                .filter(|v| **v == FreeVerdict::Freed)
                                .count() as u64,
                            Ordering::Relaxed,
                        );
                        Ok(verdicts)
                    }
                    Ok(Err(e)) => Err(WireError::from_error(&e)),
                    Err(e) => {
                        // RES-7/RES-8: the unwind is recorded — and CACHED,
                        // so a replay answers the same loud failure instead
                        // of re-running half a ladder.
                        PANICS.fetch_add(1, Ordering::Relaxed);
                        log::error!("S9 publish owner-side free execution unwound: {e}");
                        Err(WireError::from_error(&SqueezefsError::InvalidOperation(
                            format!("S9 shipped-free execution panicked: {e}"),
                        )))
                    }
                }
            })
            .await
            .clone();
        SERVED.fetch_add(1, Ordering::Relaxed);
        let frame = PublishReplyFrame {
            schema: PUBLISH_SCHEMA,
            outcome: outcome.map(PublishReply::FreeVerdicts),
        };
        match encode(&frame, "free_blocks") {
            Ok(body) => RpcResponse {
                id: req_id,
                status: PUBLISH_OK,
                body,
            },
            Err(e) => Self::refuse(req_id, PUBLISH_MALFORMED, format!("reply encode: {e}")),
        }
    }

    /// Serve one [`PublishCall::HarvestLaneFree`] through its dedup window
    /// — the [`Self::serve_free`] pattern verbatim: the winner of
    /// `(lease_epoch, request_id)` executes on the sqz-meta pool under the
    /// installed [`HarvestExecutor`]; every duplicate awaits the winner's
    /// own grant and is counted (`harvest_replays`). The lane + era gate
    /// already ran in [`Self::serve`].
    async fn serve_harvest(
        &self,
        req_id: u64,
        lease_epoch: u64,
        request_id: u64,
        call: PublishCall,
    ) -> RpcResponse {
        let PublishCall::HarvestLaneFree {
            vol_tag,
            lane,
            writers,
            max,
            ..
        } = call
        else {
            return Self::refuse(
                req_id,
                PUBLISH_MALFORMED,
                "serve_harvest dispatched a non-harvest call".into(),
            );
        };
        let Some(exec) = harvest_executor() else {
            return Self::refuse(
                req_id,
                PUBLISH_MALFORMED,
                "S9: a lane free harvest arrived but no harvest executor is installed — the \
                 ownership plane is armed without its reuse half. Handing out offsets without \
                 the allocator that owns the free list would mint two owners for one block; \
                 arm the multi-writer authority (which installs the executor beside the free \
                 executor)"
                    .to_string(),
            );
        };
        let (slot, owns) = self.harvest_dedup.slot((lease_epoch, request_id));
        if !owns {
            HARVEST_REPLAYS.fetch_add(1, Ordering::Relaxed);
        }
        let outcome = slot
            .get_or_init(|| async move {
                // The venue rule, verbatim (see serve_free).
                match crate::meta_exec::spawn_meta_join(
                    "shipped_lane_harvest",
                    exec(vol_tag, lane, writers, max, lease_epoch),
                )
                .await
                {
                    Ok(Ok(idxs)) => {
                        HARVEST_SERVED_BLOCKS.fetch_add(idxs.len() as u64, Ordering::Relaxed);
                        Ok(idxs)
                    }
                    Ok(Err(e)) => Err(WireError::from_error(&e)),
                    Err(e) => {
                        // RES-7/RES-8: recorded — and CACHED, so a replay
                        // answers the same loud failure instead of handing
                        // out half a grant twice.
                        PANICS.fetch_add(1, Ordering::Relaxed);
                        log::error!("S9 publish owner-side harvest execution unwound: {e}");
                        Err(WireError::from_error(&SqueezefsError::InvalidOperation(
                            format!("S9 lane-harvest execution panicked: {e}"),
                        )))
                    }
                }
            })
            .await
            .clone();
        SERVED.fetch_add(1, Ordering::Relaxed);
        let frame = PublishReplyFrame {
            schema: PUBLISH_SCHEMA,
            // OQ 2 (schema 8): the reply carries the authority's LIVE
            // bound age — the loop latency in force — so the co-writer's
            // refill horizon is a measurement (0 = nothing held, and the
            // ship side then keeps its derivation).
            outcome: outcome.map(|blocks| PublishReply::LaneFreeGrant {
                blocks,
                bound_age_ms: crate::free_grace::bound_age_ms(),
            }),
        };
        match encode(&frame, "harvest_lane_free") {
            Ok(body) => RpcResponse {
                id: req_id,
                status: PUBLISH_OK,
                body,
            },
            Err(e) => Self::refuse(req_id, PUBLISH_MALFORMED, format!("reply encode: {e}")),
        }
    }

    /// Rung 17 — **the custody-scoped full Put** (the s11-range leg's
    /// zeros/remove-class conviction): a shipped `SetLayoutAndSize` is
    /// computed from the SHIPPER's base view, which under two live
    /// custodians legitimately lags a peer's publishes — applying it
    /// verbatim erased the peer's newer entries while their ledger refs
    /// stayed (the C8 dangler + the double-release free refusals). The
    /// law: a RANGE holder's Put is authoritative exactly for its
    /// custody spans' blocks — inside them the Put's presence/absence is
    /// the truth (absence IS the removal intent); outside them the
    /// durable entries are preserved. A whole-file holder's (and the
    /// pre-custody publish class's) Put stays verbatim, so solo and
    /// whole-file mounts are byte-identical.
    ///
    /// Rung 18 (the zeros-interleave conviction,
    /// `.benchmarks/2026-08-17-s11-zeros-interleave-fix.md`): a RANGE
    /// holder's Put is applied **scoped or not at all**. The former
    /// no-geometry arm warned and applied VERBATIM — and since
    /// `arm_multi_writer` installed no geometry source, that arm WAS the
    /// production path: every epoch-close full Put reverted the peer's
    /// half to the shipper's stale base while the ref ops landed (the
    /// 48-finding C8 mint). The refusal is the never-lossy direction:
    /// the client's save error refills its deferred refs and the
    /// writeback ladder re-publishes; on a correctly-armed authority
    /// (the production source now installed at arm) the arm is
    /// unreachable.
    ///
    /// Rung 19 (the width-N refs composition): the SCOPED arm also
    /// returns the recomputed durable accounting — the diff of the
    /// durable head against the map it composed, through the armed
    /// [`crate::meta_backend::kv::block_refs::block_ref_resolver`]. An
    /// entry the scope DROPS contributes NOTHING: the caller's frame,
    /// staged verbatim, mints exactly one swapped pair per dropped entry
    /// (a durable-without-map take + a map-without-durable entry — the
    /// s11-blockcyclic drift arithmetic). `None` on every verbatim arm
    /// (and with no resolver armed): the caller's ops stand, byte-
    /// identical to the pre-rung-19 shape.
    ///
    /// Rung 20 residual 1 (the blob-aware compose): with the
    /// [`crate::meta_backend::kv::indirect_map::indirect_map_io`] hook
    /// armed, an INDIRECT layout on EITHER side rehydrates from its blob
    /// and the scoped compose runs over FULL maps; the composed result
    /// re-encodes inline where it fits (the collapse arm) or re-spills
    /// to a fresh CoW blob. Unarmed mounts keep the refusal verbatim.
    /// The third return value is the compose's blob custody (RES-9
    /// guard, post-commit frees, and the drop-caller-blob-ops verdict) —
    /// `Default` on every verbatim arm.
    async fn custody_scoped_layout(
        &self,
        client: &str,
        ino: u64,
        shipped: Vec<u8>,
    ) -> Result<(Vec<u8>, Option<Vec<BlockRefOp>>, ScopedBlobCustody)> {
        let Some(owner) = crate::data_grant::custody_owner() else {
            return Ok((shipped, None, ScopedBlobCustody::default()));
        };
        let spans = match owner.client_custody_on(client, ino) {
            crate::data_grant::ClientCustodyShape::Ranges(spans) => spans,
            _ => return Ok((shipped, None, ScopedBlobCustody::default())),
        };
        let block = match owner.geometry_of(ino).await {
            Some((_size, block)) if block > 0 => block,
            other => {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "S11: range holder '{client}'s full Put for ino {ino} cannot be \
                     custody-scoped ({}) — refusing rather than applying it verbatim: an \
                     unscoped Put reverts a concurrent peer's entries to this shipper's \
                     stale base while both ref streams land (the zeros-interleave C8 \
                     mint). Arm the authority's range plane with its geometry source \
                     (multi_writer::router_range_geometry)",
                    match other {
                        None => "no geometry source installed",
                        Some(_) => "the geometry source answered block size 0",
                    }
                )));
            }
        };
        use crate::meta_backend::Metadata as _;
        let durable = match self.inner.getxattr(ino, "layout").await {
            Ok(Some(bytes)) => bytes,
            // no current layout — first Put, verbatim
            _ => return Ok((shipped, None, ScopedBlobCustody::default())),
        };
        let (cur_dec, new_dec) = (
            crate::layout_wire::decode_base_layout(&durable),
            crate::layout_wire::decode_base_layout(&shipped),
        );
        // Rung 19 (the MPI-IO row's live conviction — the 10 GiB face):
        // an INDIRECT layout on EITHER side of a RANGE holder's Put is
        // un-composable at the meta plane UNARMED (the blob is a
        // data-plane read), and the retired verbatim arm replaced a
        // whole-file map with the shipper's partial view — the
        // zeros-interleave clobber at spill scale. "Scoped or not at
        // all" (rung 18's own law): REFUSE, retried-class, naming the
        // base. Rung 20 residual 1: an ARMED authority has a data router
        // (`indirect_map_io`), so either side REHYDRATES from its blob
        // and the scoped compose runs over FULL maps.
        let indirect = |r: &std::result::Result<
            crate::layout_wire::LayoutMetadata,
            crate::layout_wire::LayoutWireError,
        >| { matches!(r, Err(e) if format!("{e}").contains("indirect")) };
        let durable_indirect = indirect(&cur_dec);
        let shipped_indirect = indirect(&new_dec);
        let rehydrated = durable_indirect || shipped_indirect;
        let map_io = crate::meta_backend::kv::indirect_map::indirect_map_io();
        if rehydrated && map_io.is_none() {
            return Err(SqueezefsError::InvalidOperation(format!(
                "layout delta base unusable: indirect base — range holder '{client}'s full                  Put for ino {ino} meets an indirect map ({} side); a custody-scoped                  compose cannot read the blob at the meta plane, and verbatim apply is                  the whole-map clobber (rung 19: refetch and recompose)",
                if durable_indirect { "durable" } else { "shipped" }
            )));
        }
        // Rehydrate one indirect side: raw-decode the head, read its
        // blob, hold the FULL map inline for the compose. Failures
        // refuse loud (retried-class context) — never verbatim apply.
        let rehydrate = |bytes: Vec<u8>, side: &'static str| {
            let io = map_io.clone();
            async move {
                let io = io.ok_or_else(|| {
                    SqueezefsError::InvalidOperation(format!(
                        "S11: indirect {side} layout for ino {ino} with no indirect-map hook"
                    ))
                })?;
                let mut head = crate::layout_wire::decode_layout_any(&bytes).map_err(|e| {
                    SqueezefsError::InvalidOperation(format!(
                        "rung 20: undecodable indirect {side} layout for ino {ino}: {e}"
                    ))
                })?;
                let blob = head
                    .block_map_id
                    .as_deref()
                    .and_then(|id| id.strip_prefix("indirect:"))
                    .map(str::to_string)
                    .ok_or_else(|| {
                        SqueezefsError::InvalidOperation(format!(
                            "rung 20: indirect {side} layout for ino {ino} names no blob"
                        ))
                    })?;
                let full = (io.read)(blob.clone()).await?;
                head.block_map = Some(full.into_iter().collect());
                Ok::<_, SqueezefsError>((head, blob))
            }
        };
        // A MIXED shape (one side indirect, the other legacy/undecodable)
        // must keep the pre-compose REFUSAL posture: before rung 20 the
        // indirect refusal fired ahead of the legacy verbatim arm, and
        // relaxing it to verbatim-apply would be exactly the whole-map
        // clobber the refusal exists to prevent.
        let mixed_refusal = |side: &'static str| {
            SqueezefsError::InvalidOperation(format!(
                "S11: range holder '{client}'s Put for ino {ino} mixes an indirect layout \
                 with a legacy/undecodable {side} base — refusing rather than applying it \
                 verbatim (the whole-map clobber)"
            ))
        };
        let mut durable_old_blob: Option<String> = None;
        let mut cur = if durable_indirect {
            let (head, blob) = rehydrate(durable, "durable").await?;
            durable_old_blob = Some(blob);
            head
        } else {
            match cur_dec {
                Ok(c) => c,
                Err(_) if shipped_indirect => return Err(mixed_refusal("durable")),
                // JSON-era/undecodable base: the legacy verbatim arm.
                Err(_) => return Ok((shipped, None, ScopedBlobCustody::default())),
            }
        };
        let mut new = if shipped_indirect {
            rehydrate(shipped.clone(), "shipped").await?.0
        } else {
            match new_dec {
                Ok(n) => n,
                Err(_) if durable_indirect => return Err(mixed_refusal("shipped")),
                // JSON-era/undecodable base: the legacy verbatim arm.
                Err(_) => return Ok((shipped, None, ScopedBlobCustody::default())),
            }
        };
        let mut map = cur.block_map.take().unwrap_or_default();
        let new_map = new.block_map.take().unwrap_or_default();
        // Rung 18 (the s11-subblock C8/C2 mint): a DEMOTED region is
        // NOBODY's Put-truth — the authority is the single publisher of a
        // demoted block (KD-MW-8: every holder ships extents there and
        // never DMAs, so its RAM map for that block is legitimately
        // stale). A block overlapping a demoted region keeps its DURABLE
        // (authority-assembled) entry in both directions: the holder's
        // stale presence never overwrites it, its absence never removes
        // it.
        let demoted = crate::dlm::demoted_regions(ino);
        let in_custody = |b: u32| {
            let bs = u64::from(b) * block;
            let be = bs + block;
            spans.iter().any(|&(s, e)| e > bs && s < be)
                && !demoted.iter().any(|&(s, e)| e > bs && s < be)
        };
        // Rung 19: the pre-compose head — what the accounting diffs
        // against (the map the composition displaces FROM).
        let head = map.clone();
        map.retain(|b, _| !(in_custody(*b) && !new_map.contains_key(b)));
        // Finding 28: the caller's adopted entries pass the binding probe
        // (a dead-incarnation entry drops; the durable's stands).
        let mut adopt: Vec<(u32, String)> = new_map
            .into_iter()
            .filter(|(b, _)| in_custody(*b))
            .collect();
        retain_live_bindings(&mut adopt, ino);
        for (b, k) in adopt {
            map.insert(b, k);
        }
        // Rung 19 (the width-N refs composition): the accounting IS the
        // head→composed diff. With no resolver armed the caller's frame
        // stands (today's shape — production arm installs the resolver).
        let refs = crate::meta_backend::kv::block_refs::block_ref_resolver().map(|resolver| {
            let mut out: Vec<BlockRefOp> = Vec::new();
            let resolve =
                |key: &str, idx: u32, take: bool, out: &mut Vec<BlockRefOp>| match resolver(
                    key, ino, idx,
                ) {
                    Some(r) => out.push(if take {
                        BlockRefOp::taken(r)
                    } else {
                        BlockRefOp::released(r)
                    }),
                    None => {
                        crate::meta_backend::kv::META_KV_BLOCK_REFS_UNRESOLVED
                            .fetch_add(1, Ordering::Relaxed);
                    }
                };
            for (b, k) in &map {
                match head.get(b) {
                    Some(prev) if prev == k => {}
                    Some(prev) => {
                        resolve(prev, *b, false, &mut out);
                        resolve(k, *b, true, &mut out);
                    }
                    None => resolve(k, *b, true, &mut out),
                }
            }
            for (b, prev) in &head {
                if !map.contains_key(b) {
                    resolve(prev, *b, false, &mut out);
                }
            }
            out
        });
        // Non-map fields follow the Put (same layout class); size never
        // regresses a peer's growth (truncation is the setattr plane's).
        new.size = new.size.max(cur.size);
        new.block_map = Some(map);
        // Rung 20 residual 1: a rehydrated composition owns its own
        // naming — the re-encode decision (inline collapse vs re-spill)
        // is the owner's, derived from the SAME inline ceiling the
        // router's spill arm uses. The caller's map-blob frame ops are
        // DROPPED either way (blob custody is recomputed here); the
        // SHIPPED blob's device block is NOT freed by the owner — it
        // stays the shipper's own lifecycle (its `old_indirect_to_free`
        // tail or remount derivation reclaims it: bounded one-blob
        // residue).
        let mut blob_custody = ScopedBlobCustody::default();
        let mut refs = refs;
        if rehydrated {
            blob_custody.drop_caller_blob_ops = true;
            new.block_map_id = None;
            let inline_cap = self
                .inner
                .xattr_value_cap(ino)
                .saturating_sub(crate::routing::LAYOUT_INLINE_HEADROOM);
            let inline_encoded = crate::layout_wire::encode_layout(&new).map_err(|e| {
                SqueezefsError::InvalidOperation(format!(
                    "S11: custody-scoped layout re-encode failed for ino {ino} ({e}) — \
                     refusing rather than applying range holder '{client}'s Put verbatim \
                     (the zeros-interleave C8 mint)"
                ))
            })?;
            let encoded = if inline_encoded.len() > inline_cap {
                // Re-spill: fresh CoW blob (the hook's write FLUSHES
                // before returning — DUR-6 §3), guarded until the commit
                // names it.
                let io = map_io.ok_or_else(|| {
                    SqueezefsError::InvalidOperation(format!(
                        "S11: rehydrated compose for ino {ino} with no indirect-map hook"
                    ))
                })?;
                let full_map = new.block_map.take().unwrap_or_default();
                let mut sorted: Vec<(u32, String)> = full_map.into_iter().collect();
                sorted.sort_unstable_by_key(|&(b, _)| b);
                let (fresh, guard) = (io.write)(ino, sorted).await?;
                new.block_map_id = Some(format!("indirect:{fresh}"));
                if let Some(r) = refs.as_mut() {
                    crate::meta_backend::kv::indirect_map::push_map_blob_transfer_op(
                        r, &fresh, ino, true,
                    );
                }
                blob_custody.fresh = Some(guard);
                crate::layout_wire::encode_layout(&new).map_err(|e| {
                    SqueezefsError::InvalidOperation(format!(
                        "S11: custody-scoped layout re-encode failed for ino {ino} ({e}) — \
                         refusing rather than applying range holder '{client}'s Put verbatim \
                         (the zeros-interleave C8 mint)"
                    ))
                })?
            } else {
                inline_encoded
            };
            // Either way the composed head stops naming the DURABLE old
            // blob: release its record, free it after the commit.
            if let Some(old) = durable_old_blob {
                if let Some(r) = refs.as_mut() {
                    crate::meta_backend::kv::indirect_map::push_map_blob_transfer_op(
                        r, &old, ino, false,
                    );
                }
                blob_custody.free_after_commit.push(old);
            }
            crate::fuse_client::METRICS
                .publish_blob_composes
                .fetch_add(1, Ordering::Relaxed);
            return Ok((encoded, refs, blob_custody));
        }
        // Scoped or not at all (rung 18): a re-encode failure refuses —
        // the retired fallback applied the shipped Put verbatim, which is
        // the same peer-reverting mint the no-geometry arm minted.
        let encoded = crate::layout_wire::encode_layout(&new).map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "S11: custody-scoped layout re-encode failed for ino {ino} ({e}) — refusing \
                 rather than applying range holder '{client}'s Put verbatim (the \
                 zeros-interleave C8 mint)"
            ))
        })?;
        // Finding 21 (PR 5 attempt 2's A1 killer): the NON-rehydrated
        // composition can cross the value cap even when both inputs
        // individually respected it — 32 scoped writers of one shared
        // file compose here, and the verbatim inline encode was a
        // permanent fsync failure (the KV refuses the record, the
        // never-lossy retry recomposes the same over-cap value for
        // ever; on a volume whose admission passes it, the record is
        // the 2026-08-19 checkpoint-wedge mint instead). Same ceiling,
        // same spill, as the rehydrated arm above.
        let inline_cap = self
            .inner
            .xattr_value_cap(ino)
            .saturating_sub(crate::routing::LAYOUT_INLINE_HEADROOM);
        if encoded.len() > inline_cap {
            let io = map_io.ok_or_else(|| {
                SqueezefsError::InvalidOperation(format!(
                    "S11: composed layout for ino {ino} crosses the inline ceiling \
                     ({} B > {inline_cap} B) with no indirect-map hook — refusing \
                     rather than staging the over-cap record (finding 21's \
                     permanent-retry class)",
                    encoded.len()
                ))
            })?;
            let full_map = new.block_map.take().unwrap_or_default();
            let mut sorted: Vec<(u32, String)> = full_map.into_iter().collect();
            sorted.sort_unstable_by_key(|&(b, _)| b);
            let (fresh, guard) = (io.write)(ino, sorted).await?;
            new.block_map_id = Some(format!("indirect:{fresh}"));
            if let Some(r) = refs.as_mut() {
                crate::meta_backend::kv::indirect_map::push_map_blob_transfer_op(
                    r, &fresh, ino, true,
                );
            }
            blob_custody.fresh = Some(guard);
            let encoded = crate::layout_wire::encode_layout(&new).map_err(|e| {
                SqueezefsError::InvalidOperation(format!(
                    "S11: custody-scoped layout re-encode failed for ino {ino} ({e}) — \
                     refusing rather than applying range holder '{client}'s Put verbatim \
                     (the zeros-interleave C8 mint)"
                ))
            })?;
            crate::fuse_client::METRICS
                .publish_compose_spills
                .fetch_add(1, Ordering::Relaxed);
            return Ok((encoded, refs, blob_custody));
        }
        Ok((encoded, refs, blob_custody))
    }

    async fn execute(&self, client: &str, call: PublishCall) -> Result<PublishReply> {
        match call {
            PublishCall::SetLayoutAndSize {
                ino,
                layout,
                size,
                refs,
                ..
            } => {
                let refs: Vec<BlockRefOp> = refs.into_iter().map(BlockRefOp::from).collect();
                let (layout, recomputed, mut blob_custody) =
                    self.custody_scoped_layout(client, ino, layout).await?;
                // Rung 19: on the SCOPED arm the accounting is the
                // composition's own diff; the caller's MAP-BLOB ops (the
                // indirect blob custody transfer — index-disjoint from
                // map entries) travel verbatim beside it — EXCEPT on the
                // rung-20 compose arm, where the owner recomputes blob
                // custody entirely and the caller's blob ops are DROPPED
                // (staged verbatim they double-count / mis-name blobs
                // the composition renamed). Every verbatim arm keeps the
                // caller's frame byte-identical.
                let refs: Vec<BlockRefOp> = match recomputed {
                    Some(mut r) => {
                        if !blob_custody.drop_caller_blob_ops {
                            r.extend(refs.iter().filter(|o| o.reference.is_map_blob()).copied());
                        }
                        r
                    }
                    None => refs,
                };
                self.inner
                    .set_layout_and_size(ino, &layout, size, &refs)
                    .await?;
                // Rung 20: the commit named the fresh blob — custody
                // transfers (an error above dropped the guard ARMED,
                // freeing the fresh blob) — and the displaced durable
                // blob is freed only NOW (the DUR-6 CoW law).
                if let Some(g) = blob_custody.fresh.as_mut() {
                    g.disarm();
                }
                if !blob_custody.free_after_commit.is_empty() {
                    if let Some(io) = crate::meta_backend::kv::indirect_map::indirect_map_io() {
                        for blob_key in blob_custody.free_after_commit {
                            (io.free)(blob_key).await;
                        }
                    }
                }
                Ok(PublishReply::Unit)
            }
            PublishCall::MergeLayoutAndSize {
                ino,
                delta,
                full_layout,
                size,
                refs,
                ..
            } => {
                let mut delta = crate::layout_wire::LayoutDelta::decode(&delta).map_err(|e| {
                    SqueezefsError::InvalidOperation(format!(
                        "S9 publish: undecodable layout delta for ino {ino}: {e}"
                    ))
                })?;
                // Finding 28: a stale caller entry never regresses a
                // block to a dead binding — filter the delta AND the
                // re-base fallback map (per-entry drop, never a frame
                // refusal; the shipper's cache heals via the served-
                // layout invalidation it already rides).
                let full_layout = {
                    let mut full_layout = full_layout;
                    if retain_live_bindings(&mut delta.entries, ino) > 0 {
                        if let Ok(mut base) = crate::layout_wire::decode_base_layout(&full_layout) {
                            if let Some(map) = base.block_map.as_mut() {
                                let mut ents: Vec<(u32, String)> = map.drain().collect();
                                retain_live_bindings(&mut ents, ino);
                                *map = ents.into_iter().collect();
                            }
                            if let Ok(re) = bincode::serialize(&base) {
                                full_layout = re;
                            }
                        }
                    }
                    full_layout
                };
                let refs: Vec<BlockRefOp> = refs.into_iter().map(BlockRefOp::from).collect();
                // Rung 17 (KD-MW-8's composition law): a SHIPPED merge
                // CHAINS ONTO THE DURABLE HEAD — the claim re-stamps
                // under the backend's own 4a I-guard and the link
                // version re-mints from THIS authority's sequencer, so
                // two co-writers' publishes of one ino compose instead
                // of refusing (the refetch wedge) or re-basing with a
                // private full layout (the s11-range C8 clobber). The
                // reply carries the staged version: the co-writer chains
                // without a refetch.
                let (used, version) = self
                    .inner
                    .merge_layout_and_size_chained(
                        ino,
                        &delta,
                        bytes::Bytes::from(full_layout),
                        size,
                        refs,
                    )
                    .await?;
                Ok(PublishReply::DeltaUsed { used, version })
            }
            PublishCall::WriteExtent {
                ino,
                block_index,
                offset_in_block,
                data,
                token,
                lease_epoch,
                request_id,
            } => {
                let Some(exec) = extent_merge_executor() else {
                    return Err(SqueezefsError::InvalidOperation(
                        "S11: a shipped extent arrived but no assembler executor is \
                         installed — the ownership plane is armed without its assembly \
                         half; arm the multi-writer authority (which installs the extent \
                         merge executor beside the free executor)"
                            .to_string(),
                    ));
                };
                let covering = exec(ExtentFrame {
                    client: client.to_string(),
                    ino,
                    block_index,
                    offset_in_block,
                    data,
                    token,
                    lease_epoch,
                    request_id,
                })
                .await?;
                // The coverage ledger: merged now; covered iff the
                // executor proved a covering publish already ran.
                crate::extent_ship::owner_note_merge(client, ino, request_id);
                if let Some(v) = covering {
                    crate::extent_ship::owner_note_covered(ino, v);
                }
                Ok(PublishReply::ExtentAck {
                    covering_version: covering,
                })
            }
            PublishCall::FlushExtents { ino, .. } => {
                let Some(exec) = extent_flush_executor() else {
                    return Err(SqueezefsError::InvalidOperation(
                        "S11: a FlushExtents force arrived but no assembler flush executor \
                         is installed — the ownership plane is armed without its assembly \
                         half; arm the multi-writer authority"
                            .to_string(),
                    ));
                };
                let covering_version = exec(ino).await?;
                // Every extent merged before this force is covered by the
                // publish the force just committed.
                crate::extent_ship::owner_note_covered(ino, covering_version);
                Ok(PublishReply::FlushDone { covering_version })
            }
            PublishCall::CommitBlockRefs { ino, refs, .. } => {
                let refs: Vec<BlockRefOp> = refs.into_iter().map(BlockRefOp::from).collect();
                self.inner.commit_block_refs(ino, &refs).await?;
                Ok(PublishReply::Unit)
            }
            PublishCall::ParkWriteTimes {
                ino, mtime, ctime, ..
            } => {
                self.inner.park_write_times(ino, mtime, ctime).await?;
                Ok(PublishReply::Unit)
            }
            PublishCall::DestroyInodes { inos, .. } => {
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
                ..
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
            PublishCall::BlockRefPopulation {
                vol_tag,
                block_idxs,
            } => {
                // Finding 13: answered from the volumes THIS node holds
                // authority over — its own peer-owned copies are lagged
                // snapshots whose truth belongs to their owners, so they
                // are deliberately not counted here.
                let mut out = vec![0u64; block_idxs.len()];
                for (v_idx, kv) in self.inner.volumes.iter().enumerate() {
                    if !self.authority.get(v_idx).copied().unwrap_or(false) {
                        continue;
                    }
                    for (slot, idx) in block_idxs.iter().enumerate() {
                        out[slot] += kv.block_ref_count(vol_tag, *idx).await.map_err(|e| {
                            SqueezefsError::InvalidOperation(format!(
                                "S9: the block-reference population read failed on {} while \
                                 serving a shipped-free validation: {e}",
                                kv.device_path().display()
                            ))
                        })? as u64;
                    }
                }
                Ok(PublishReply::Populations(out))
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
            PublishCall::FreeBlocks { .. } => {
                // Unreachable by construction: `serve` routes every free
                // through `serve_free`'s era gate + dedup window. Refusing
                // (never executing) keeps that construction a fact rather
                // than a convention.
                Err(SqueezefsError::InvalidOperation(
                    "S9: free_blocks is served only through the dedup window (serve_free) — \
                     dispatching it here would bypass the exactly-once witness"
                        .to_string(),
                ))
            }
            PublishCall::HarvestLaneFree { .. } => {
                // Same construction as the free above: only serve_harvest.
                Err(SqueezefsError::InvalidOperation(
                    "S9: harvest_lane_free is served only through the dedup window \
                     (serve_harvest) — dispatching it here would bypass the exactly-once witness"
                        .to_string(),
                ))
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
