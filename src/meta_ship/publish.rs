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
//! * **Framing, not a batching conveyor** (E2E perf audit D-1b, 2026-09-02
//!   — the row asked: 24 concurrent publishes from one co-writer were 24
//!   stop-and-wait frames and 24 owner conveyor passes). The client keeps
//!   S8's lane shape per endpoint: callers enqueue and park, a drain takes
//!   **everything queued** into ONE [`PublishRequestFrame`] the moment a
//!   frame slot is free (no timer, no added delay — a serial stream still
//!   pays one RTT per publish), and up to `depth` frames are in flight per
//!   endpoint on a pool of that many sessions (the wire is request/reply
//!   per connection on both ends; single-connection multiplexing is the
//!   audit's D-5). The owner partitions a frame into dependency chains by
//!   named-inode overlap (D-1's `chains_by_named_inos`), runs chains
//!   concurrently and same-ino calls in submission order, so a frame's
//!   independent publishes co-queue into ONE M7 pass. Every law below is
//!   PER CALL: the era gate, the not-owner screen, the witness window, the
//!   RETRIED/REFUSED classes, the `recomputed` verdict, and the reply
//!   shape — a frame is homogeneous in resend class (the drain cuts a
//!   frame where the class changes), so a transport resend re-sends the
//!   SAME frame with the SAME request ids or nothing.
//! * **No cross-owner transaction.** `destroy_inodes` is per-ino by
//!   construction and groups by owner; nothing here spans two authorities
//!   inside one transaction (that is S3.5, and S8's `cross_owner_error` is
//!   the refusal wherever it can be reached).
//!
//! [`super::ownership_armed`]: crate::meta_ship::ownership_armed
//! [`PublishRequestFrame`]: crate::meta_ship::publish::PublishRequestFrame

use super::service::DedupWindow;
use super::wire::{WireDirEntry, WireError, WireInode};
use crate::cluster_wire::{
    RpcAsyncService, RpcClient, RpcRequest, RpcResponse, RPC_OK, RPC_UNKNOWN_VERB,
};
use crate::error::{Result, SqueezefsError};
use crate::meta_backend::kv::block_refs::{BlockRef, BlockRefOp};
use crate::meta_backend::{DirEntry, Ino, Inode, LayoutPublish, RoutedMetaBackend};
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
///
/// **9 since the merge reply carries the owner-recompute verdict**
/// (finding 36): [`PublishReply::DeltaUsed`] gained `recomputed` — `true`
/// ⇔ the owner REPLACED the caller's accounting frame with the rung-19/20
/// recompute and ran the released blocks through its OWN free ladder
/// post-commit, so the co-writer must stand its caller-frame displaced
/// frees down (shipping them is the refusal/leak pair the finding named:
/// a free whose block was already freed refuses on the untracked
/// tripwire, and the block the commit ACTUALLY displaced leaks). An
/// 8-speaker cannot decode the widened reply — the mismatch refuses loud
/// at the first frame (KD-7 same-commit fleets).
///
/// **10 since the SCOPED PUT reply carries the same verdict** (finding
/// 36b — the s11-mpiio field venue): on a range-custody fleet a shared
/// file's saves are FULL-SAVE class (indirect-classed RAM heads are never
/// delta-eligible), so the recompute the co-writer must stand down for is
/// `custody_scoped_layout`'s — the `SetLayoutAndSize` serve — not the
/// merge arms schema 9 covered. `SetLayoutAndSize` now answers
/// [`PublishReply::PutDone`] (`recomputed` beside the commit), and the
/// owner frees the scoped compose's released data blocks through its own
/// ladder post-commit, exactly the schema-9 law on the arm the field
/// actually rides.
///
/// **11 since the kvmap crossing verb landed** (PB-class files PR 2,
/// design-kvmap-block-map-tree §3 + A4): `MigrateBlockMap` — a
/// co-writer's beyond-inline crossing ships its WHOLE map and the OWNER
/// runs the migration train under its own held 4a + serve stripe —
/// plus [`PublishReply::MapMigrated`], the train's accounting. A
/// 10-speaker cannot decode either — the mismatch refuses loud at the
/// first frame.
///
/// **12 since the claims-scoped served train landed** (kvmap PR 5b,
/// design-kvmap-block-map-tree §11 laws a+b): `MigrateBlockMap` gained
/// `base_gen` (the shipper's known map generation — the belt input a
/// lagging ship refuses on, retried-class), and
/// [`PublishReply::MapMigrated`] gained the f36b verdict triple
/// (`recomputed` — the owner replaced the shipper's accounting frame with
/// the train's own swap diff and freed the TRUE displaced set itself, so
/// the caller's frame-derived displaced frees stand down — plus the
/// `released` count and the committed `gen` the shipper stamps as its
/// next base). An 11-speaker cannot decode either direction — the
/// mismatch refuses loud at the first frame (KD-7 same-commit fleets).
///
/// **13 since the publish frame carries N calls** (E2E perf audit D-1b —
/// `docs/design-e2e-perf-audit.md` §3 board DLM #8, D-1's scoping
/// finding): [`PublishRequestFrame::calls`] is a `Vec` and
/// [`PublishReplyFrame::outcomes`] answers one [`PublishCallOutcome`]
/// per call in call order, so the per-call refusal statuses
/// (`PUBLISH_NOT_OWNER` / `PUBLISH_STALE_LEASE` / `PUBLISH_LANE_REFUSED`
/// / `PUBLISH_PANIC`) moved from the wire status into the reply body —
/// the wire status now covers only frame-level refusals (schema,
/// malformed). A 12-speaker reads a `Vec` where it expects one call in
/// either direction — the mismatch refuses loud at the first frame
/// (KD-7 same-commit fleets).
///
/// **14 since the harvest reply carries each granted block's release age**
/// (finding 15 term 2, `.benchmarks/2026-09-06-free-grace-lane-visible.md`):
/// [`PublishReply::LaneFreeGrant`] gained `release_ages_ms` — per block,
/// the ms it sat on the authority's free list since its grace release,
/// measured on the AUTHORITY's clock — so the co-writer can stamp the
/// `released_served` stage of `alloc_lane_visible_phase_ns` beside its
/// own round trip without either node reading the other's clock. A
/// 13-speaker cannot decode the widened reply — the mismatch refuses loud
/// at the first frame (KD-7 same-commit fleets), the schema-8 posture
/// verbatim.
///
/// **15 since the recompute replies carry the authority's FREED offsets**
/// (`.benchmarks/2026-09-07-cowriter-claim-anomaly-lineage.md`):
/// [`PublishReply::PutDone`], [`PublishReply::DeltaUsed`] and
/// [`PublishReply::MapMigrated`] gained `freed` — per served publish, the
/// `(vol_tag, block_idx)` of every block the owner's recompute ladder
/// answered `Freed` (the `MapMigrated` count became the list). Schemas
/// 9/10/12 told the co-writer to stand its frame-derived frees DOWN but
/// never WHICH offsets the authority had free-listed, so a parked
/// rewrite-epoch predecessor's local tracking waited for the epoch CLOSE
/// — and on the fleet the grace ring → lane harvest → claim loop beat the
/// close (`block_claim_anomalies`, ~1 % of every recompute-freed block).
/// A 14-speaker would read the widened reply as nothing freed and keep the
/// lineage — the mismatch refuses loud at the first frame instead.
///
/// **16 since every reply frame carries the authority's LANE-FREE NOTICES**
/// (`.benchmarks/2026-09-07-cowriter-claim-anomaly-population.md`): the
/// fleet's `block_claim_anomalies` population was not the served arm's at
/// all — it is the authority's OWN publishes displacing a co-writer's
/// blocks (the assembler's fold of the co-writer's shipped slices), frees
/// no served reply can carry because no call of the co-writer's produced
/// them. [`PublishReplyFrame::lane_frees`] drains, per reply, the
/// [`WireLaneFree`] notices queued for the frame's client since its last
/// reply — queued BEFORE the authority's ladder runs, so the reply that
/// hands an offset back through a harvest was built after its notice was
/// queued and carries it — and [`PublishReply::LaneFreeGrant`] gained
/// `grant_seq`, the per-client grant sequence a notice's `after_grants` is
/// ordered against (a notice below an offset's grant sequence names a
/// lifetime the co-writer already re-minted, and touches nothing). A
/// 15-speaker would read the notices as absent and keep the lineage — the
/// mismatch refuses loud at the first frame (KD-7 same-commit fleets).
pub const PUBLISH_SCHEMA: u32 = 16;

/// First verb of S9's publish block. S3's ping is 0, S8's metadata verbs
/// are 16/17, S6's membership owns `0x0100..=0x01FF`, S9's custody
/// `0x0200..=0x02FF`; the publish path takes `0x0300..=0x03FF`.
pub const VERB_PUBLISH_BASE: u16 = 0x0300;
/// Last verb of S9's publish block.
pub const VERB_PUBLISH_LAST: u16 = 0x03FF;
/// One publish FRAME per wire call — a frame carries one or more
/// [`PublishCall`]s (see the module docs on framing).
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

/// One device block the owner's recompute ladder answered `Freed` for
/// (schema 15) — the durable identity (KD-5's `vol_tag`, the block index),
/// never a key or a path: the co-writer resolves it against its OWN
/// allocator and parked keys, which is where the lifetime stamp lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireFreedBlock {
    pub vol_tag: u64,
    pub block_idx: u64,
}

/// One **lane-free notice** (schema 16): a block of the receiving client's
/// LANE whose reference the authority's OWN publish released — a free the
/// authority performs on the client's behalf that no served reply can
/// name (the assembler's fold of the client's shipped slices; a peer's
/// explicit free of a block the client minted). The client releases its
/// local tracking of the offset — the non-accounting hygiene, the
/// `retire_displaced_locally` decrement — unless the offset's harvest grant
/// sequence is ABOVE `after_grants` (the client re-minted it from a grant
/// the authority served after queuing this notice; the reply carrying the
/// notice was reordered behind the grant's), in which case the live
/// lifetime is untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireLaneFree {
    pub vol_tag: u64,
    pub block_idx: u64,
    /// The client's [`PublishReply::LaneFreeGrant`] sequence at the moment
    /// the notice was queued: every grant that can hand this offset back
    /// was served after, so it carries a higher `grant_seq`.
    pub after_grants: u64,
}

/// The finding-36 verdict a shipped layout publish answers with, as the
/// shipper consumes it: `recomputed` (the frame-derived free stream stands
/// down) plus, since schema 15, the offsets the owner's ladder actually
/// FREED — the co-writer's local-hygiene input
/// (`DataRouter::retire_recomputed_parked`). Every LOCAL
/// arm answers `Default` (its recompute's released set travels as
/// `BlockRef`s for the caller's own ladder instead).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OwnerVerdict {
    pub recomputed: bool,
    pub freed: Vec<WireFreedBlock>,
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
    /// **The kvmap crossing train, shipped** (PB-class files PR 2 —
    /// design-kvmap-block-map-tree §3/A4, the finding-36b whole-claim-set
    /// law): a co-writer cannot commit metadata, so its beyond-inline
    /// crossing ships the WHOLE map (and its whole refs frame — the
    /// owner runs the journal-entry-cap chunking, f38's law on the node
    /// whose journal admits the commit) and the OWNER executes the train
    /// under its own held 4a + `SERVE_INO_LOCKS` stripe.
    ///
    /// Layout-publish class: era-gated + witnessed
    /// (`(lease_epoch, request_id)`) — the train is idempotent by the A1
    /// diff, but only the witness makes a lost-reply resend answer the
    /// winner's own outcome instead of re-running a whole train.
    MigrateBlockMap {
        ino: u64,
        /// The encoded `kvmap:1` head layout the flip commits.
        layout: Vec<u8>,
        size: u64,
        /// The COMPLETE map, `(block_index, block-key string)`, sorted.
        entries: Vec<(u32, String)>,
        refs: Vec<WireBlockRefOp>,
        /// PR 5b (design §11's belt): the map generation of the head this
        /// ship was computed against. A sticky-head train whose base
        /// generation lags the durable head refuses retried-class — the
        /// shipper refetches and recomposes (the `admit_versioned_delta`
        /// stage/rebase/refuse pattern). 0 on a first crossing (no kvmap
        /// head exists to lag).
        base_gen: u64,
        /// Era gate input + witness half (see `SetLayoutAndSize`).
        lease_epoch: u64,
        /// The witness's other half.
        request_id: u64,
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
            Self::MigrateBlockMap { .. } => "migrate_block_map",
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
            | Self::FlushExtents { lease_epoch, .. }
            | Self::MigrateBlockMap { lease_epoch, .. } => Some(*lease_epoch),
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
            | Self::FlushExtents { lease_epoch, .. }
            | Self::MigrateBlockMap { lease_epoch, .. } => Some(*lease_epoch),
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
                | Self::MigrateBlockMap { .. }
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
            }
            | Self::MigrateBlockMap {
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
            | Self::MigrateBlockMap { ino, .. }
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
    /// `set_layout_and_size` (schema 10, finding 36b): the commit landed,
    /// and `recomputed` says whether the owner REPLACED the caller's
    /// accounting frame with the custody-scoped compose's own swap diff —
    /// `true` ⇔ the owner ran the released blocks through its own free
    /// ladder post-commit, so the caller's frame-derived displaced frees
    /// must stand down for this publish. Since schema 15 `freed` names the
    /// blocks that ladder answered `Freed` — the ones now in the owner's
    /// free supply, which the co-writer's lane harvest can hand back — so
    /// the shipper retires its local tracking of exactly those at THIS
    /// reply (never at the epoch close the harvest can beat).
    PutDone {
        recomputed: bool,
        freed: Vec<WireFreedBlock>,
    },
    /// `merge_layout_and_size`: whether a delta record was staged, and
    /// the STAGED LINK'S VERSION (0 on a full-Put commit) — rung 17's
    /// chain-without-refetch input (design-mw-layout-versions §6's named
    /// residual): the co-writer stamps its RAM provenance from the reply
    /// so its next delta claims the right base with no round trip.
    DeltaUsed {
        used: bool,
        version: u64,
        /// Finding 36 (schema 9): `true` ⇔ the owner recomputed the staged
        /// accounting against its own head (rung 19/20) and owns the
        /// displaced-block DEVICE frees — the co-writer's caller-frame
        /// free stream must stand down for this publish (local tier /
        /// tracking hygiene only).
        recomputed: bool,
        /// Schema 15: the recompute ladder's `Freed` blocks (see
        /// [`PublishReply::PutDone`]).
        freed: Vec<WireFreedBlock>,
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
    /// Since schema 14 it also carries, per granted block in `blocks`
    /// order, the ms that block sat on the authority's free list since
    /// its grace release (`release_ages_ms`,
    /// [`crate::free_grace::LANE_RELEASE_AGE_UNPLACED`] = no mark) — the
    /// lane-visible ledger's `released_served` stage, measured on the
    /// authority's clock and stamped by the co-writer beside its own
    /// round trip (finding 15 term 2). Since schema 16 `grant_seq` is the
    /// authority's per-client grant sequence this grant was served at —
    /// the ordering witness a [`WireLaneFree`] notice's `after_grants` is
    /// compared against.
    LaneFreeGrant {
        blocks: Vec<u64>,
        bound_age_ms: u64,
        release_ages_ms: Vec<u64>,
        grant_seq: u64,
    },
    /// `block_ref_population`: per-index reference populations summed over
    /// the SERVING node's owned volumes, in request order.
    Populations(Vec<u64>),
    /// `migrate_block_map` (kvmap PR 2): the served train's accounting —
    /// the shipper's ledger counters read the OWNER's truth, and
    /// `preexisting` is the A1 resumed verdict's input. Since schema 12
    /// (PR 5b) the reply also carries the f36b verdict: `recomputed`
    /// ⇔ the owner replaced the shipper's accounting frame with the
    /// claims-scoped train's own swap diff and ran the displaced blocks
    /// through its OWN free ladder post-commit — the caller's
    /// frame-derived displaced frees must stand down; `freed` (schema 15,
    /// the schema-12 count became the list) the blocks that ladder
    /// answered `Freed` (see [`PublishReply::PutDone`]); and `gen`, the
    /// committed head generation the shipper stamps as its next base (the
    /// §11 belt's chain-without-refetch input).
    MapMigrated {
        records: u64,
        record_bytes: u64,
        preexisting: u64,
        recomputed: bool,
        freed: Vec<WireFreedBlock>,
        gen: u64,
    },
}

/// A publish request frame: one or more calls from ONE client (schema 13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishRequestFrame {
    pub schema: u32,
    /// The client's identity (logs and audit only — authentication is the
    /// transport's, and the storage-trust secret is the root).
    pub client: String,
    /// The frame's calls, in submission order; the reply answers one
    /// [`PublishCallOutcome`] per call in the same order. Never empty.
    pub calls: Vec<PublishCall>,
}

/// One call's answer inside a [`PublishReplyFrame`] (schema 13): the
/// per-call face of what used to be the wire status + body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PublishCallOutcome {
    /// The call was executed; this is its own outcome, errno-preserving.
    Done(std::result::Result<PublishReply, WireError>),
    /// The call was REFUSED before execution (or its execution unwound):
    /// one of the `PUBLISH_*` statuses other than [`PUBLISH_OK`], with
    /// the operator-facing reason. Nothing was applied.
    Refused { status: u16, detail: String },
}

/// A publish reply frame: one outcome per call, in call order, plus the
/// lane-free notices queued for the frame's client since its last reply
/// (schema 16 — see [`WireLaneFree`]; empty on every frame to a client
/// whose lane blocks no authority publish displaced).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishReplyFrame {
    pub schema: u32,
    pub outcomes: Vec<PublishCallOutcome>,
    pub lane_frees: Vec<WireLaneFree>,
}

/// The upper bound the frame-forming drain uses for one call's encoded
/// size: bincode's varint ints are never longer than 9 bytes, so every
/// integer counts 9 and every sequence its length prefix plus its
/// elements. Exact enough to keep a frame inside the CONTROL cap without
/// encoding it twice; a single call past the cap still ships alone and
/// refuses at encode as before.
const INT_HINT: usize = 9;

impl PublishCall {
    /// See [`INT_HINT`].
    fn wire_size_hint(&self) -> usize {
        fn refs(n: usize) -> usize {
            INT_HINT + n * (4 * INT_HINT + 1)
        }
        fn bytes(n: usize) -> usize {
            INT_HINT + n
        }
        // The enum tag.
        INT_HINT
            + match self {
                Self::SetLayoutAndSize {
                    layout, refs: r, ..
                } => 3 * INT_HINT + bytes(layout.len()) + refs(r.len()),
                Self::MergeLayoutAndSize {
                    delta,
                    full_layout,
                    refs: r,
                    ..
                } => 3 * INT_HINT + bytes(delta.len()) + bytes(full_layout.len()) + refs(r.len()),
                Self::CommitBlockRefs { refs: r, .. } => 3 * INT_HINT + refs(r.len()),
                Self::ParkWriteTimes { .. } => 4 * INT_HINT,
                Self::DestroyInodes { inos, .. } => 2 * INT_HINT + inos.len() * INT_HINT,
                Self::CreateWithRdevSize { name, .. } => 7 * INT_HINT + bytes(name.len()),
                Self::XattrValueCap { .. } => INT_HINT,
                Self::ReaddirStream { .. } => 3 * INT_HINT,
                Self::RaiseAllocLane { .. } => 5 * INT_HINT,
                Self::FreeBlocks { blocks, .. } => {
                    3 * INT_HINT + INT_HINT + blocks.len() * INT_HINT
                }
                Self::HarvestLaneFree { .. } => 6 * INT_HINT,
                Self::WriteExtent { data, .. } => 5 * INT_HINT + bytes(data.len()),
                Self::FlushExtents { .. } => 3 * INT_HINT,
                Self::BlockRefPopulation { block_idxs, .. } => {
                    INT_HINT + INT_HINT + block_idxs.len() * INT_HINT
                }
                Self::MigrateBlockMap {
                    layout,
                    entries,
                    refs: r,
                    ..
                } => {
                    4 * INT_HINT
                        + bytes(layout.len())
                        + INT_HINT
                        + entries
                            .iter()
                            .map(|(_, k)| INT_HINT + bytes(k.len()))
                            .sum::<usize>()
                        + refs(r.len())
                }
            }
    }
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

/// Encode a request frame (trusted input — we built it).
pub fn encode_request_frame(frame: &PublishRequestFrame) -> Result<Vec<u8>> {
    encode(frame, "frame")
}

/// Decode a request frame (**untrusted** — bounded by the CONTROL cap;
/// the `publish_wire` fuzz target's entry).
pub fn decode_request_frame(bytes: &[u8]) -> Result<PublishRequestFrame> {
    decode(bytes, "request")
}

/// Encode a reply frame.
pub fn encode_reply_frame(frame: &PublishReplyFrame) -> Result<Vec<u8>> {
    encode(frame, "reply frame")
}

/// Decode a reply frame (**untrusted** — bounded; a co-writer trusts an
/// authority's reply no further than the authority trusts its request).
pub fn decode_reply_frame(bytes: &[u8]) -> Result<PublishReplyFrame> {
    decode(bytes, "reply frame")
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
static FREE_REFUSED_BLOCKS: AtomicU64 = AtomicU64::new(0);
static FREE_RECOMPUTED_BLOCKS: AtomicU64 = AtomicU64::new(0);
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
// Finding 34: the custody-less-Put shield's engagement (must stay 0).
static UNSCOPED_PUT_REFUSALS: AtomicU64 = AtomicU64::new(0);
// kvmap PR 2 — the shipped crossing's ledger (design §5:
// `meta_ship_publish.map_{shipped,served,refused}`).
static MAP_SHIPPED: AtomicU64 = AtomicU64::new(0);
static MAP_SERVED: AtomicU64 = AtomicU64::new(0);
static MAP_REFUSED: AtomicU64 = AtomicU64::new(0);
// kvmap PR 5b (design §11 law a — the f36b twin): displaced blocks a
// served claims-scoped train's recompute RELEASED, whose free ladder ran
// on this authority (the engagement gauge).
static MAP_RECOMPUTED_RELEASES: AtomicU64 = AtomicU64::new(0);
// D-1b — the publish plane's framing ledger (the S8 `batches` /
// `batched_verbs` shape): client-side frames shipped and the calls they
// carried (`ship_framed_calls / ship_frames` is the live coalesce
// factor), drain parks on the depth bound, and the owner's frames /
// calls / dependency chains served (`served_chains / served_frames` is
// the live chain width — ≈ frame width = fully independent).
static SHIP_FRAMES: AtomicU64 = AtomicU64::new(0);
static SHIP_FRAMED_CALLS: AtomicU64 = AtomicU64::new(0);
static SHIP_DEPTH_WAITS: AtomicU64 = AtomicU64::new(0);
static SHIP_SESSION_DIALS: AtomicU64 = AtomicU64::new(0);
// D-5 — frames shipped on a MULTIPLEXED session (the single-connection
// lever's engagement: ≡ `ship_frames` under
// `SQUEEZEFS_PUBLISH_SHIP_MULTIPLEX=1`, 0 on the shipped pool).
static SHIP_MUX_FRAMES: AtomicU64 = AtomicU64::new(0);
static SERVED_FRAMES: AtomicU64 = AtomicU64::new(0);
static SERVED_FRAME_CALLS: AtomicU64 = AtomicU64::new(0);
static SERVED_CHAINS: AtomicU64 = AtomicU64::new(0);
// D-1c — served frames that committed ≥ 1 conveyor GROUP (the round
// dispatch's engagement gauge; the group population itself is the
// process-global `META_CONVEYOR_GROUP_{COMMITS,TXS}`).
static FRAME_GROUPS: AtomicU64 = AtomicU64::new(0);

/// The D-1c A/B lever: group a served frame's independent layout
/// publishes into ONE conveyor enqueue per round (default on). `0` = the
/// pre-rung per-call path — the measurement control, never an operational
/// escape. Read per served frame (one getenv per wire round trip) so a
/// contract can flip it in-process.
pub const CONVEYOR_GROUP_ENV: &str = "SQUEEZEFS_PUBLISH_CONVEYOR_GROUP";

fn conveyor_group_enabled() -> bool {
    crate::env_knobs::bool_knob(CONVEYOR_GROUP_ENV, true)
}

/// The at-budget W2 spill's counter (incremented by
/// [`crate::extent_ship`]'s spill arm — release path 4's engagement).
pub(crate) fn note_extent_spill() {
    EXTENT_SPILLS.fetch_add(1, Ordering::Relaxed);
}

/// PR 5b (design §11's belt): the train's generation-lag refusal is
/// minted inside the backend (under the held 4a, where the durable gen is
/// race-free) — this is its `map_refused` row.
pub(crate) fn note_map_refused() {
    MAP_REFUSED.fetch_add(1, Ordering::Relaxed);
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
    /// Shipped frees this owner answered `Refused` — the double-release
    /// lineage (the block was already free, graced, quarantined,
    /// mid-reclaim, or names a dead lifetime of a reallocated offset). The
    /// owner-side face of the shipper's refusal log: `served + refused +
    /// non-terminal ≡ the peers' shipped`, and a stream of it beside a
    /// healthy rewrite is a duplicate free ISSUER on some peer, never a
    /// leak (the refusal is the leak-safe arm) — finding 15's blob-reclaim
    /// storm was 3,448 of these against 3,914 served.
    pub free_refused_blocks: u64,
    /// Finding 36 (owner side): recompute-released device frees — blocks a
    /// served chained/composed merge's rung-19/20 recompute RELEASED whose
    /// terminal ladder ran on this authority post-commit (`Freed` verdicts
    /// only). The rewriting-fleet engagement gauge: 0 beside a growing
    /// recomputed-merge stream means displaced frees are leaking again.
    pub free_recomputed_blocks: u64,
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
    /// Lane-free notices queued for co-writers (schema 16): blocks of a
    /// co-writer's lane an authority publish displaced — the assembler's
    /// folds on the fleet. ≈ the authority's `fold_passes` on a fpp row.
    pub lane_free_notices_queued: u64,
    /// Notices drained into reply frames; `queued − shipped` at quiesce is
    /// the backlog of clients that sent no frame since.
    pub lane_free_notices_shipped: u64,
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
    /// Finding 34: range-custody-less full Puts REFUSED on an ino other
    /// writers hold ranges on (the verbatim-clobber shield). **Must stay
    /// 0** — growth means some client shipped a layout publish after its
    /// range release outran it (the rung-1 drain ordering broken).
    pub unscoped_put_refusals: u64,
    /// kvmap crossings SHIPPED to an owner (client side).
    pub map_shipped: u64,
    /// kvmap crossing trains EXECUTED for a peer (owner side) —
    /// `shipped ≡ served` is the engagement law.
    pub map_served: u64,
    /// kvmap map-plane refusals: shipped crossings the owner refused (its
    /// tree could not engage, or the ino carries live range grants — the
    /// finding-34 posture), LOCAL whole-map trains refused under live
    /// range grants (§11 row 4 — the f34 screen's local twin), and scoped
    /// Puts refused over a `kvmap:` head (§11 row 2 — the S11 ∘ kvmap
    /// compose lands in PR 5b). The client's never-lossy ladder
    /// re-publishes.
    pub map_refused: u64,
    /// kvmap PR 5b (the f36b twin): displaced blocks a served
    /// claims-scoped train's recompute released, freed through THIS
    /// authority's own ladder post-commit — the rewriting-kvmap-fleet
    /// engagement gauge (0 beside a growing recomputed-train stream
    /// means displaced frees are leaking again).
    pub map_recomputed_releases: u64,
    /// D-1b (client side): publish FRAMES shipped — the wire round trips.
    pub ship_frames: u64,
    /// D-1b (client side): calls carried by those frames;
    /// `ship_framed_calls / ship_frames` is the live coalesce factor
    /// (≈ 1 on a serial stream — one RTT per publish, the accepted cost;
    /// ≫ 1 on a concurrent one).
    pub ship_framed_calls: u64,
    /// D-1b (client side): drain parks on the per-endpoint depth bound —
    /// the K+1'th frame waiting. Growth means the pipe is saturated at
    /// the current depth (the frames that follow carry more calls each).
    pub ship_depth_waits: u64,
    /// D-1b (client side): publish sessions dialed — pool growth toward
    /// the depth bound plus reconnects after a transport failure / the
    /// owner's idle reaper. Bounded by `depth` per endpoint at steady
    /// state; steady growth means sessions are dying between frames.
    pub ship_session_dials: u64,
    /// D-5 (client side): frames shipped on the endpoint's ONE pipelined
    /// session (`SQUEEZEFS_PUBLISH_SHIP_MULTIPLEX=1`) — ≡ `ship_frames`
    /// with the lever engaged; 0 on the shipped session pool. With it,
    /// `ship_session_dials` reads 1 per endpoint at steady state instead
    /// of `depth`.
    pub ship_mux_frames: u64,
    /// D-1b (owner side): publish frames served.
    pub served_frames: u64,
    /// D-1b (owner side): calls those frames carried (`shipped ≡ served`
    /// composes per call; this is the frame-level face).
    pub served_frame_calls: u64,
    /// D-1b (owner side): dependency chains dispatched — the concurrent
    /// units. `served_chains / served_frames` ≈ frame width means the
    /// frame's calls were independent (co-queue into one M7 pass); ≈ 1
    /// means one hot object serialized the frame.
    pub served_chains: u64,
    /// D-1c (owner side): served frames that committed at least one
    /// conveyor GROUP — a round's independent layout publishes staged
    /// together and enqueued under one queue lock (one apply pass by
    /// construction). ≈ `served_frames` minus the single-call frames on a
    /// grouping authority; 0 under `SQUEEZEFS_PUBLISH_CONVEYOR_GROUP=0`.
    pub frame_groups: u64,
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
        free_refused_blocks: FREE_REFUSED_BLOCKS.load(Ordering::Relaxed),
        free_recomputed_blocks: FREE_RECOMPUTED_BLOCKS.load(Ordering::Relaxed),
        free_replays: FREE_REPLAYS.load(Ordering::Relaxed),
        free_stale_refusals: FREE_STALE_REFUSALS.load(Ordering::Relaxed),
        free_ship_failures: FREE_SHIP_FAILURES.load(Ordering::Relaxed),
        harvest_shipped_blocks: HARVEST_SHIPPED_BLOCKS.load(Ordering::Relaxed),
        harvest_served_blocks: HARVEST_SERVED_BLOCKS.load(Ordering::Relaxed),
        lane_free_notices_queued: LANE_FREE_NOTICES_QUEUED.load(Ordering::Relaxed),
        lane_free_notices_shipped: LANE_FREE_NOTICES_SHIPPED.load(Ordering::Relaxed),
        harvest_replays: HARVEST_REPLAYS.load(Ordering::Relaxed),
        harvest_refusals: HARVEST_REFUSALS.load(Ordering::Relaxed),
        extent_shipped: EXTENT_SHIPPED.load(Ordering::Relaxed),
        extent_served: EXTENT_SERVED.load(Ordering::Relaxed),
        extent_replays: EXTENT_REPLAYS.load(Ordering::Relaxed),
        extent_stale_refusals: EXTENT_STALE_REFUSALS.load(Ordering::Relaxed),
        extent_flush_forces: EXTENT_FLUSH_FORCES.load(Ordering::Relaxed),
        extent_spills: EXTENT_SPILLS.load(Ordering::Relaxed),
        unscoped_put_refusals: UNSCOPED_PUT_REFUSALS.load(Ordering::Relaxed),
        map_shipped: MAP_SHIPPED.load(Ordering::Relaxed),
        map_served: MAP_SERVED.load(Ordering::Relaxed),
        map_refused: MAP_REFUSED.load(Ordering::Relaxed),
        map_recomputed_releases: MAP_RECOMPUTED_RELEASES.load(Ordering::Relaxed),
        ship_frames: SHIP_FRAMES.load(Ordering::Relaxed),
        ship_framed_calls: SHIP_FRAMED_CALLS.load(Ordering::Relaxed),
        ship_depth_waits: SHIP_DEPTH_WAITS.load(Ordering::Relaxed),
        ship_session_dials: SHIP_SESSION_DIALS.load(Ordering::Relaxed),
        ship_mux_frames: SHIP_MUX_FRAMES.load(Ordering::Relaxed),
        served_frames: SERVED_FRAMES.load(Ordering::Relaxed),
        served_frame_calls: SERVED_FRAME_CALLS.load(Ordering::Relaxed),
        served_chains: SERVED_CHAINS.load(Ordering::Relaxed),
        frame_groups: FRAME_GROUPS.load(Ordering::Relaxed),
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
        "free_refused_blocks": s.free_refused_blocks,
        "free_recomputed_blocks": s.free_recomputed_blocks,
        "free_replays": s.free_replays,
        "free_stale_refusals": s.free_stale_refusals,
        "free_ship_failures": s.free_ship_failures,
        "harvest_shipped_blocks": s.harvest_shipped_blocks,
        "harvest_served_blocks": s.harvest_served_blocks,
        "lane_free_notices_queued": s.lane_free_notices_queued,
        "lane_free_notices_shipped": s.lane_free_notices_shipped,
        "harvest_replays": s.harvest_replays,
        "harvest_refusals": s.harvest_refusals,
        "extent_shipped": s.extent_shipped,
        "extent_served": s.extent_served,
        "extent_replays": s.extent_replays,
        "extent_stale_refusals": s.extent_stale_refusals,
        "extent_flush_forces": s.extent_flush_forces,
        "extent_spills": s.extent_spills,
        "unscoped_put_refusals": s.unscoped_put_refusals,
        "map_shipped": s.map_shipped,
        "map_served": s.map_served,
        "map_refused": s.map_refused,
        "map_recomputed_releases": s.map_recomputed_releases,
        "ship_frames": s.ship_frames,
        "ship_framed_calls": s.ship_framed_calls,
        "ship_depth_waits": s.ship_depth_waits,
        "ship_session_dials": s.ship_session_dials,
        "ship_mux_frames": s.ship_mux_frames,
        "served_frames": s.served_frames,
        "served_frame_calls": s.served_frame_calls,
        "served_chains": s.served_chains,
        "frame_groups": s.frame_groups,
        // §9.3's live retention gauge (→ 0 at quiesce — falsifiable
        // against the four release paths).
        "extent_retained_bytes": crate::extent_ship::retained_bytes(),
    })
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

/// **Test seam** (the S8 `TEST_SHIP_DRAIN_HOLD_MS` precedent): milliseconds
/// the publish lane's drain waits at its head before taking the queue, so
/// concurrent arrivals accumulate deterministically and the framing
/// contract is testable without a sleep as coordination. `0` = off.
pub static TEST_PUBLISH_DRAIN_HOLD_MS: AtomicU64 = AtomicU64::new(0);

/// Absolute override for the derived per-endpoint in-flight frame depth.
pub const SHIP_DEPTH_ENV: &str = "SQUEEZEFS_PUBLISH_SHIP_DEPTH";

/// The per-endpoint in-flight frame depth (D-1b): how many publish frames
/// one client keeps in flight to one authority — one authenticated session
/// each, since the cluster wire is request/reply per connection on both
/// ends.
///
/// Derivation (caps derive from system resources): `ceil(cpus / 8)`
/// clamped to `[2, 8]` — the SAME "one lane per 8 cores" slope the owner's
/// own RPC-lane derivation uses ([`crate::cluster_wire::service_threads_from`]),
/// so a client's sessions toward one authority scale with the box the way
/// the authority's serving lanes do. Floor 2 = the minimum at which a
/// frame's wire RTT overlaps a sibling frame's owner pass at all (depth 1
/// serializes RTT + pass per frame); ceiling 8 = the RPC-lane ceiling, so
/// a 256-core client does not open a session farm per authority. `cpus` is
/// the fleet-share-divided root (KD-MW-14).
pub fn publish_ship_depth() -> usize {
    publish_ship_depth_from(
        crate::env_knobs::opt_int_knob::<usize>(SHIP_DEPTH_ENV),
        crate::cpu::process_parallelism(),
    )
}

/// Pure form (tie-tested in the derivation sweep): explicit wins verbatim
/// within its admissible range (`1` = stop-and-wait, the A/B control);
/// derived = `ceil(cpus / 8).clamp(2, 8)`.
pub fn publish_ship_depth_from(explicit: Option<usize>, cpus: usize) -> usize {
    if let Some(explicit) = explicit {
        return explicit.clamp(1, 64);
    }
    cpus.div_ceil(8).clamp(2, 8)
}

/// D-5 (DLM #8's single-connection half): the in-flight frames ride ONE
/// pipelined session per authority ([`crate::cluster_wire::MuxSession`] —
/// replies demultiplexed by id, the owner's session lane serving them
/// concurrently) instead of one request/reply session each. `depth` keeps
/// its meaning (frames in flight); what it no longer costs is a
/// connection per frame — F-B's `max_connections` cap counts sessions, so
/// a co-writer at depth 8 held 8 of the authority's slots.
///
/// **Ships OFF on measurement** (`.benchmarks/2026-09-08-d5-owner-hop-and-
/// depth.md`): the connection thread is the execution venue, so one
/// session serves its K in-flight frames on ONE owner thread where the
/// pool served them on K — in-process at equal depth 4 the pool ran ≈ 1.5×
/// the multiplexed publish rate (16 A-B-B-A legs, four rolls). The lever
/// is the CAPABILITY arm for fleets where the authority's connection cap
/// binds before its CPU does; `1` engages it, `0`/unset = the D-1b pool.
pub const SHIP_MULTIPLEX_ENV: &str = "SQUEEZEFS_PUBLISH_SHIP_MULTIPLEX";

/// Read the lever (once per publish lane, at its first frame).
fn publish_ship_multiplex() -> bool {
    crate::env_knobs::bool_knob(SHIP_MULTIPLEX_ENV, false)
}

/// The per-frame call cap — S8's frame cap, for the same reason: a frame's
/// calls become that many transactions on the owner's conveyor, so sizing a
/// frame past what one pass drains buys queueing, not throughput.
fn frame_call_cap() -> usize {
    super::router::batch_max()
}

/// The frame's byte budget: the wire's CONTROL class cap less the frame
/// header (`schema` + `client` + the calls' length prefix, all ≤
/// [`INT_HINT`] each plus the client id).
fn frame_byte_budget(client_len: usize) -> usize {
    (decode_limit() as usize).saturating_sub(4 * INT_HINT + client_len)
}

/// One queued publish: the call, its parking spot, and what the shipper
/// needs to frame and answer it without re-inspecting the call.
struct Submission {
    call: PublishCall,
    /// [`PublishCall::transport_resend_safe`] — frames are homogeneous in
    /// this, so a resend re-sends a whole frame or nothing.
    resend_safe: bool,
    size_hint: usize,
    reply: squeezefs_ipc::sqz_channel::oneshot::Sender<Result<PublishReply>>,
    queued_at: std::time::Instant,
}

/// One endpoint's lane: the bounded submission queue its drain serves.
struct PublishLane {
    tx: squeezefs_ipc::sqz_channel::mpsc::Sender<Submission>,
}

/// What every frame shipper on one lane shares: the endpoint, the
/// identity, the depth bound and the sessions — the idle-session POOL (≤
/// depth request/reply sessions by construction — only a permit holder
/// ever dials one), or under [`SHIP_MULTIPLEX_ENV`] ONE pipelined session
/// every in-flight frame rides.
struct LaneShared {
    endpoint: String,
    peer_id: Arc<str>,
    secret: Arc<Vec<u8>>,
    depth: Arc<squeezefs_ipc::sqz_semaphore::Semaphore>,
    pool: parking_lot::Mutex<Vec<RpcClient>>,
    /// D-5: frames multiplexed on one socket (`true`) or one session per
    /// in-flight frame (`false`, the D-1b pool). Read once per lane.
    multiplex: bool,
    /// The lane's one pipelined session (`multiplex`); replaced when dead.
    mux: parking_lot::Mutex<Option<Arc<crate::cluster_wire::MuxSession>>>,
}

/// The client half: per authority endpoint, one framing lane and a pool of
/// up to `depth` authenticated sessions, kept warm.
pub struct PublishClient {
    peer_id: Arc<str>,
    secret: Arc<Vec<u8>>,
    /// Frames this client may hold in flight per endpoint.
    depth: usize,
    lanes: scc::HashMap<String, Arc<PublishLane>>,
}

impl std::fmt::Debug for PublishClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublishClient")
            .field("peer_id", &self.peer_id)
            .field("depth", &self.depth)
            .field("lanes", &self.lanes.len())
            .finish_non_exhaustive()
    }
}

impl PublishClient {
    /// A client identifying itself as `peer_id`, proving storage membership
    /// with the volume set's `job:enroll` secret, at the derived depth
    /// ([`publish_ship_depth`]).
    pub fn new(peer_id: &str, secret: Vec<u8>) -> Arc<Self> {
        Self::with_depth(peer_id, secret, publish_ship_depth())
    }

    /// [`Self::new`] with an explicit per-endpoint in-flight frame depth
    /// (the measurement lever; `1` = stop-and-wait).
    pub fn with_depth(peer_id: &str, secret: Vec<u8>, depth: usize) -> Arc<Self> {
        Arc::new(Self {
            peer_id: Arc::from(peer_id),
            secret: Arc::new(secret),
            depth: depth.max(1),
            lanes: scc::HashMap::new(),
        })
    }

    /// The lane for `endpoint`, started on first use.
    fn lane(&self, endpoint: &str) -> Arc<PublishLane> {
        if let Some(lane) = self.lanes.read_sync(endpoint, |_, l| Arc::clone(l)) {
            return lane;
        }
        // Bounded by law (S8's lane): frame_cap × 8 submissions in flight,
        // so a saturated authority backpressures its clients instead of
        // growing a queue without limit; a stuck one surfaces as the
        // wire's reply timeout on the frame, never as unbounded queueing.
        let (tx, rx) =
            squeezefs_ipc::sqz_channel::mpsc::channel::<Submission>(frame_call_cap() * 8);
        let lane = Arc::new(PublishLane { tx });
        let spawn_drain = || {
            let shared = Arc::new(LaneShared {
                endpoint: endpoint.to_string(),
                peer_id: Arc::clone(&self.peer_id),
                secret: Arc::clone(&self.secret),
                depth: Arc::new(squeezefs_ipc::sqz_semaphore::Semaphore::new(self.depth)),
                pool: parking_lot::Mutex::new(Vec::with_capacity(self.depth)),
                multiplex: publish_ship_multiplex(),
                mux: parking_lot::Mutex::new(None),
            });
            // The drain rides the sqz-meta pool — the venue that owns the
            // daemon's plane tasks; it ends when the client (and hence the
            // lane's sender) is dropped.
            crate::meta_exec::spawn_meta("meta_ship_publish_drain", lane_drain(shared, rx));
        };
        match self
            .lanes
            .insert_sync(endpoint.to_string(), Arc::clone(&lane))
        {
            Ok(()) => {
                spawn_drain();
                lane
            }
            Err(_) => match self.lanes.read_sync(endpoint, |_, l| Arc::clone(l)) {
                Some(raced_in) => raced_in,
                None => {
                    spawn_drain();
                    lane
                }
            },
        }
    }

    /// Ship one call to `endpoint` and return its outcome.
    ///
    /// The call joins the endpoint's lane and travels in the next frame
    /// the drain forms (alone on a quiet lane — no delay is ever added;
    /// beside every concurrently queued call on a busy one). Every law
    /// below is applied per call by the frame shipper:
    ///
    /// * **Resend** (rung 18, residual d): a TRANSPORT failure on a frame
    ///   of resend-safe calls reconnects and resends the SAME frame once
    ///   (same request ids — the owner's dedup window absorbs it); a frame
    ///   of one-attempt calls (the un-witnessed mutators) refuses on the
    ///   true sent-then-lost ambiguity. The drain never mixes the two.
    ///   The witnessed layout-publish class rides `ship_witnessed`'s
    ///   bounded epoch-stable ladder above this.
    /// * **Era** (finding #6): a per-call [`PUBLISH_STALE_LEASE`] whose
    ///   refused epoch IS this client's current lease composes the full
    ///   fence HERE (`note_publish_era_refused` — the pull-based
    ///   revocation law at the publish round trip), surfacing as
    ///   `WriterGuardFenced`.
    pub async fn ship(&self, endpoint: &str, call: PublishCall) -> Result<PublishReply> {
        let lane = self.lane(endpoint);
        let (tx, rx) = squeezefs_ipc::sqz_channel::oneshot::channel();
        lane.tx
            .send(Submission {
                resend_safe: call.transport_resend_safe(),
                size_hint: call.wire_size_hint(),
                call,
                reply: tx,
                queued_at: std::time::Instant::now(),
            })
            .await
            .map_err(|_| {
                SqueezefsError::InvalidOperation(format!(
                    "S9: the publish lane to {endpoint} is gone"
                ))
            })?;
        rx.await.map_err(|_| {
            SqueezefsError::InvalidOperation(format!(
                "S9: the publish lane to {endpoint} dropped a frame's outcomes"
            ))
        })?
    }
}

/// One lane's drain: form frames from whatever is queued the moment a
/// frame slot is free, and hand each to its own shipper task.
///
/// The park on the depth bound comes BEFORE the frame is formed, so the
/// frame that goes out when a slot frees carries everything that queued
/// while the pipe was full — the natural batching of a busy pipe, with no
/// timer and no added delay on a quiet one. A frame is cut at the call
/// cap, at the byte budget, and where the resend class changes (the item
/// that would have crossed a boundary heads the next frame).
async fn lane_drain(
    shared: Arc<LaneShared>,
    mut rx: squeezefs_ipc::sqz_channel::mpsc::Receiver<Submission>,
) {
    let mut carry: Option<Submission> = None;
    loop {
        let first = match carry.take() {
            Some(s) => s,
            None => match rx.recv().await {
                Some(s) => s,
                None => return,
            },
        };
        // The head-of-iteration hold is a TEST seam only (0 in production,
        // one relaxed load).
        let hold = TEST_PUBLISH_DRAIN_HOLD_MS.load(Ordering::Relaxed);
        if hold > 0 {
            squeezefs_ipc::sqz_time::sleep(std::time::Duration::from_millis(hold)).await;
        }
        let permit = match Arc::clone(&shared.depth).try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                SHIP_DEPTH_WAITS.fetch_add(1, Ordering::Relaxed);
                match Arc::clone(&shared.depth).acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => {
                        // The semaphore is never closed; refuse loud
                        // rather than strand the waiter if it ever is.
                        let _ = first
                            .reply
                            .send(Err(SqueezefsError::InvalidOperation(format!(
                                "S9: the publish lane to {} lost its depth bound",
                                shared.endpoint
                            ))));
                        return;
                    }
                }
            }
        };
        let cap = frame_call_cap();
        let budget = frame_byte_budget(shared.peer_id.len());
        let class = first.resend_safe;
        let mut bytes = first.size_hint;
        let mut frame = vec![first];
        while frame.len() < cap {
            match rx.try_recv() {
                Ok(next) => {
                    if next.resend_safe != class || bytes.saturating_add(next.size_hint) > budget {
                        carry = Some(next);
                        break;
                    }
                    bytes += next.size_hint;
                    frame.push(next);
                }
                Err(_) => break,
            }
        }
        let shared = Arc::clone(&shared);
        crate::meta_exec::spawn_meta("meta_ship_publish_frame", async move {
            ship_frame(&shared, frame).await;
            drop(permit);
        });
    }
}

/// A parked caller: its reply slot plus what the per-call interpretation
/// needs (the verb name for messages, the presented epoch for the fence).
struct Waiter {
    reply: squeezefs_ipc::sqz_channel::oneshot::Sender<Result<PublishReply>>,
    name: &'static str,
    presented: Option<u64>,
}

fn fail_all(waiters: Vec<Waiter>, msg: &str) {
    for w in waiters {
        let _ = w
            .reply
            .send(Err(SqueezefsError::InvalidOperation(msg.to_string())));
    }
}

/// Ship one formed frame and fan every call's outcome back to its caller.
async fn ship_frame(shared: &LaneShared, frame: Vec<Submission>) {
    let n = frame.len();
    let resend_safe = frame.first().is_some_and(|s| s.resend_safe);
    let mut calls = Vec::with_capacity(n);
    let mut waiters = Vec::with_capacity(n);
    for sub in frame {
        super::phase_record(super::ShipPhase::QueueWait, sub.queued_at);
        waiters.push(Waiter {
            reply: sub.reply,
            name: sub.call.name(),
            presented: sub.call.presented_epoch(),
        });
        calls.push(sub.call);
    }
    SHIP_FRAMES.fetch_add(1, Ordering::Relaxed);
    SHIP_FRAMED_CALLS.fetch_add(n as u64, Ordering::Relaxed);
    let body = match encode_request_frame(&PublishRequestFrame {
        schema: PUBLISH_SCHEMA,
        client: shared.peer_id.to_string(),
        calls,
    }) {
        Ok(b) => b,
        Err(e) => {
            fail_all(waiters, &e.to_string());
            return;
        }
    };
    let reply = match shared.exchange(body, resend_safe, n).await {
        Ok(r) => r,
        Err(e) => {
            fail_all(waiters, &e.to_string());
            return;
        }
    };
    // Calls that travelled to an owner — per call, whatever the status
    // (today's ledger semantics, one frame later).
    SHIPPED.fetch_add(n as u64, Ordering::Relaxed);
    if reply.status != PUBLISH_OK {
        // A FRAME-level refusal (schema / malformed): every call shares it.
        fail_all(
            waiters,
            &format!(
                "S9: the owner at {} refused a {n}-call publish frame (status {}): {}",
                shared.endpoint,
                reply.status,
                String::from_utf8_lossy(&reply.body)
            ),
        );
        return;
    }
    let decoded = match decode_reply_frame(&reply.body) {
        Ok(f) => f,
        Err(e) => {
            fail_all(waiters, &e.to_string());
            return;
        }
    };
    if decoded.schema != PUBLISH_SCHEMA {
        fail_all(
            waiters,
            &format!(
                "S9: the owner at {} replied in publish schema {} (this build speaks \
                 {PUBLISH_SCHEMA})",
                shared.endpoint, decoded.schema
            ),
        );
        return;
    }
    if decoded.outcomes.len() != n {
        fail_all(
            waiters,
            &format!(
                "S9 publish protocol violation: the owner at {} answered a {n}-call frame with \
                 {} outcomes — refusing every call rather than guessing which is whose",
                shared.endpoint,
                decoded.outcomes.len()
            ),
        );
        return;
    }
    // Schema 16: the frame's lane-free notices apply BEFORE any outcome
    // reaches its caller — a harvest grant in this frame is adopted only
    // after the notices its blocks' frees queued have released this mount's
    // stale tracking of them.
    crate::cowriter::apply_lane_free_notices(&decoded.lane_frees);
    for (w, outcome) in waiters.into_iter().zip(decoded.outcomes) {
        let out = match outcome {
            PublishCallOutcome::Done(r) => r.map_err(WireError::into_error),
            PublishCallOutcome::Refused { status, detail } if status == PUBLISH_STALE_LEASE => {
                let fenced = match (w.presented, crate::data_grant::custody_client()) {
                    (Some(epoch), Some(client)) => client.note_publish_era_refused(epoch, &detail),
                    _ => false,
                };
                log::error!(
                    "S9: the authority at {} refused {} BY ERA ({detail}) — nothing was \
                     applied{}",
                    shared.endpoint,
                    w.name,
                    if fenced {
                        "; this epoch was our CURRENT lease, so the full fence composed \
                         (custody poisoned — re-admission is by remount)"
                    } else {
                        " (the refused epoch is not this mount's current lease — a dead \
                         frame, not a dead era)"
                    }
                );
                Err(SqueezefsError::WriterGuardFenced)
            }
            PublishCallOutcome::Refused { status, detail } => {
                Err(SqueezefsError::InvalidOperation(format!(
                    "S9: the owner at {} refused {} (status {status}): {detail}",
                    shared.endpoint, w.name
                )))
            }
        };
        let _ = w.reply.send(out);
    }
}

impl LaneShared {
    /// One authenticated exchange of an encoded frame: a pooled session
    /// (or a fresh dial), the round trip, and the session's return to the
    /// pool on success. A transport failure drops the session; a
    /// resend-safe frame then reconnects and resends the SAME bytes once
    /// (rung 18, residual d — the idle-session reaper makes a dead first
    /// session the normal state of a frame that follows a quiet spell); a
    /// one-attempt frame refuses (the true sent-then-lost ambiguity).
    async fn exchange(
        &self,
        mut body: Vec<u8>,
        resend_safe: bool,
        n: usize,
    ) -> Result<RpcResponse> {
        let attempts = if resend_safe { 2 } else { 1 };
        let mut last_err: Option<SqueezefsError> = None;
        for attempt in 0..attempts {
            let bytes = if attempt + 1 == attempts {
                std::mem::take(&mut body)
            } else {
                body.clone()
            };
            // Rung 9 (S8-a attribution): a publish frame pays the same
            // authenticated round trip as an S8 verb frame, so it records
            // the SAME `meta_ship_phase_ns.rtt` phase.
            let out = if self.multiplex {
                self.exchange_multiplexed(bytes).await
            } else {
                self.exchange_pooled(bytes).await
            };
            match out {
                Ok(r) => return Ok(r),
                Err(e) => {
                    if attempt + 1 < attempts {
                        log::warn!(
                            "S9: a {n}-call publish frame to {} failed ({e}) — reconnecting and \
                             resending the same frame (resend-safe class: witnessed / monotone \
                             / read; the idle-session reaper makes a dead first session normal)",
                            self.endpoint
                        );
                    }
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            SqueezefsError::InvalidOperation(format!(
                "S9: no session to {} and no error to report for a {n}-call frame",
                self.endpoint
            ))
        }))
    }

    /// One attempt on the D-1b session POOL: a pooled request/reply session
    /// (or a fresh dial), the round trip, and the session's return to the
    /// pool on success; a transport failure drops the session.
    async fn exchange_pooled(&self, bytes: Vec<u8>) -> Result<RpcResponse> {
        // Finding 14: a POOLED session the idle reaper closed is provably
        // dead BEFORE the send — replacing it here costs no attempt and
        // touches no retry law. The pool guard is a statement-scoped short
        // hold, never across an await.
        let pooled = self.pool.lock().pop();
        let mut session = match pooled {
            Some(s) if !s.dead_on_arrival() => s,
            _ => {
                let c =
                    RpcClient::connect(&self.endpoint, &self.secret, &self.peer_id, None).await?;
                SHIP_SESSION_DIALS.fetch_add(1, Ordering::Relaxed);
                c
            }
        };
        let t_rtt = std::time::Instant::now();
        let out = session.call(VERB_PUBLISH_CALL, bytes).await;
        super::phase_record(super::ShipPhase::Rtt, t_rtt);
        if out.is_ok() {
            self.pool.lock().push(session);
        }
        out
    }

    /// One attempt on the lane's ONE pipelined session (D-5): the frame
    /// travels beside every other in-flight frame on the same socket; a
    /// dead session (a transport failure on any frame, the owner's idle
    /// reaper) is replaced by the next frame's dial — a racing pair of
    /// dials keeps the first one stored (a wasted dial, never a wrong
    /// session).
    async fn exchange_multiplexed(&self, bytes: Vec<u8>) -> Result<RpcResponse> {
        let live = self.mux.lock().clone().filter(|s| !s.is_dead());
        let session = match live {
            Some(s) => s,
            None => {
                let fresh = crate::cluster_wire::MuxSession::connect(
                    &self.endpoint,
                    &self.secret,
                    &self.peer_id,
                    None,
                )
                .await?;
                SHIP_SESSION_DIALS.fetch_add(1, Ordering::Relaxed);
                let mut slot = self.mux.lock();
                match slot.as_ref().filter(|s| !s.is_dead()) {
                    Some(raced_in) => Arc::clone(raced_in),
                    None => {
                        *slot = Some(Arc::clone(&fresh));
                        fresh
                    }
                }
            }
        };
        SHIP_MUX_FRAMES.fetch_add(1, Ordering::Relaxed);
        let t_rtt = std::time::Instant::now();
        let out = session.call(VERB_PUBLISH_CALL, bytes).await;
        super::phase_record(super::ShipPhase::Rtt, t_rtt);
        out
    }
}

static CLIENT: Lazy<arc_swap::ArcSwapOption<PublishClient>> =
    Lazy::new(arc_swap::ArcSwapOption::empty);

/// Rung 17: the OWNER-side per-ino serialization of served layout-class
/// verbs (design §6a law 3's owner half, made structural): the
/// custody-scoped full Put reads the durable layout BEFORE its commit,
/// and two clients' serves of one ino must not interleave in that window
/// (the backend's own 4a guard covers the commit, not the read). Striped;
/// collisions only serialize spuriously — but the hold spans a served
/// publish's read → compose → COMMIT, so a stripe-mate pays a full commit
/// for nothing: D-3 sizes the table by the DLM width law
/// (`stripe_locks::lease_stripe_width` — `SQUEEZEFS_DLM_STRIPES` explicit,
/// else `next_pow2(max(1024, 16 × possible_cpus × q_depth))`) and every
/// acquisition goes through [`serve_ino_guard`] /
/// [`serve_ino_guard_by_index`], the census doors
/// (`serve_ino_stripe_collisions` vs `serve_ino_key_waits`).
static SERVE_INO_LOCKS: Lazy<crate::stripe_locks::StripeLocks<crate::sqz_sync::SqzMutex<()>>> =
    Lazy::new(|| crate::stripe_locks::StripeLocks::new(crate::stripe_locks::lease_stripe_width()));

static SERVE_INO_CENSUS_TABLE: Lazy<crate::stripe_locks::StripeCensus> = Lazy::new(|| {
    crate::stripe_locks::StripeCensus::new(
        SERVE_INO_LOCKS.width(),
        &crate::stripe_locks::SERVE_INO_CENSUS,
    )
});

/// The serve stripe population in force.
pub fn serve_ino_stripe_width() -> usize {
    SERVE_INO_LOCKS.width()
}

/// The serve stripe `ino` maps to (public: the census contract suite
/// constructs stripe-mates; `run_layout_group` dedupes by it).
#[inline]
pub fn serve_ino_stripe(ino: u64) -> usize {
    SERVE_INO_LOCKS.shard_index(ino)
}

/// The ONE acquisition door for `ino`'s serve stripe.
pub async fn serve_ino_guard(ino: u64) -> crate::sqz_sync::SqzMutexGuard<'static, ()> {
    serve_ino_guard_by_index(serve_ino_stripe(ino), ino).await
}

/// Acquire serve stripe `stripe` on behalf of `ino` (the census identity —
/// for a deduped group, the first member of the stripe). `try_lock` first
/// (the same one core critical section the parking acquire takes
/// uncontended); a refusal is the contended arm: classify against the
/// stripe's last acquirer, then park.
pub async fn serve_ino_guard_by_index(
    stripe: usize,
    ino: u64,
) -> crate::sqz_sync::SqzMutexGuard<'static, ()> {
    let key = crate::stripe_locks::key_word(ino, 0);
    let lock = SERVE_INO_LOCKS.get_by_index(stripe);
    let g = match lock.try_lock() {
        Ok(g) => g,
        Err(_) => {
            SERVE_INO_CENSUS_TABLE.classify_contended(stripe, key);
            lock.lock().await
        }
    };
    SERVE_INO_CENSUS_TABLE.stamp(stripe, key);
    g
}

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
    // Finding 35b: the routing save's compose window already HOLDS this
    // ino's stripe for the whole fetch→compose→commit span — re-acquiring
    // here would self-deadlock (the stripe is not re-entrant).
    if serve_window_already_held() {
        return None;
    }
    if crate::data_grant::custody_owner().is_some() && crate::dlm::ino_has_range_custody(ino) {
        Some(serve_ino_guard(ino).await)
    } else {
        None
    }
}

squeezefs_ipc::sqz_task_local! {
    /// Finding 35b: marks a task that already holds the ino's serve
    /// stripe — the routing save's compose window spans fetch → compose →
    /// commit, and the publish layer's own guard must stand down inside
    /// it instead of self-deadlocking. Task-scoped like [`ARBITER_FOLD`].
    static SERVE_WINDOW_HELD: ();
}

/// Is the current task inside a held serve window (finding 35b)?
pub(crate) fn serve_window_already_held() -> bool {
    SERVE_WINDOW_HELD.try_with(|_| ()).is_ok()
}

/// Finding 35b: does `ino`'s layout publish commit LOCALLY on this
/// mount? The routing save's compose window is an AUTHORITY-side act —
/// a save that SHIPS must never hold the serve stripe across the wire
/// (the owner's serve of that very publish parks on the same stripe:
/// the ladder-suite self-deadlock).
pub(crate) fn publishes_locally(be: &Arc<RoutedMetaBackend>, ino: Ino) -> bool {
    owner_of_unchecked(be, ino).is_none()
}

/// Acquire `ino`'s serve stripe for a routing-side compose window
/// (finding 35b): the AUTHORITY's local save of a range-episode ino must
/// read the durable head, compose its claims onto it, and commit — all
/// under the SAME stripe hold the served scoped Puts serialize on, or a
/// serve landing mid-window is clobbered by the save's pre-serve
/// snapshot (the aged-file strand cluster: six takes stranded at one
/// staging instant).
pub(crate) async fn hold_serve_window(ino: Ino) -> crate::sqz_sync::SqzMutexGuard<'static, ()> {
    serve_ino_guard(ino).await
}

/// Run `f` with the held-serve-window marker set (finding 35b — the
/// caller holds the guard from [`hold_serve_window`] across it).
pub(crate) async fn with_serve_window_held<F: std::future::Future>(f: F) -> F::Output {
    SERVE_WINDOW_HELD.scope((), f).await
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

/// Run `f` under the arbiter-fold scope (see `ARBITER_FOLD`).
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

/// Routed [`RoutedMetaBackend::set_layout_and_size`]. Returns the
/// finding-36b owner-recompute verdict ([`OwnerVerdict`]): `recomputed`
/// ⇔ the owner's custody-scoped compose replaced the caller's accounting
/// frame and ran the displaced-block device frees through its own ladder,
/// so the caller's frame-derived displaced frees must stand down for this
/// publish; `freed` the blocks that ladder free-listed (schema 15). The
/// local arm answers `Default` (its own free path stays authoritative).
pub async fn set_layout_and_size(
    be: &Arc<RoutedMetaBackend>,
    ino: Ino,
    layout: &[u8],
    size: u64,
    refs: &[BlockRefOp],
) -> Result<OwnerVerdict> {
    match owner_of(be, ino)? {
        None => {
            note_local();
            let _serve_window = local_publish_guard(ino).await;
            be.set_layout_and_size(ino, layout, size, refs).await?;
            Ok(OwnerVerdict::default())
        }
        Some(peer) => {
            intent_barrier_inos(&[ino]).await?;
            match ship_witnessed(
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
            .await?
            {
                PublishReply::PutDone { recomputed, freed } => {
                    Ok(OwnerVerdict { recomputed, freed })
                }
                other => Err(protocol_error(
                    "set_layout_and_size",
                    &format!("{other:?}"),
                    "a Put acknowledgement",
                )),
            }
        }
    }
}

/// Routed [`RoutedMetaBackend::merge_layout_and_size`]. Returns
/// `(use_delta, staged_version, owner_verdict, local_released)` — the
/// staged link's version (0 on a full-Put commit), which the caller stamps
/// into the RAM provenance so the next delta claims the right base (rung
/// 17's chain-without-refetch law; on the un-chained local arm the version
/// is the delta's own), plus the finding-36 [`OwnerVerdict`]: `recomputed`
/// ⇔ the publish's accounting was RECOMPUTED, so the caller's
/// frame-derived displaced frees must stand down, and `freed` the blocks
/// a SHIPPED recompute's owner free-listed (schema 15). The fourth element
/// is the LOCAL chained arm's released set (finding 36b): the shipped
/// arm's owner freed its own, so it travels back empty; a local
/// recompute's releases must run the shipped-free ladder at the save's
/// post-guard venue (RES-1: never inline — the caller may hold the 3.5
/// stripe).
pub async fn merge_layout_and_size(
    be: &Arc<RoutedMetaBackend>,
    ino: Ino,
    delta: &crate::layout_wire::LayoutDelta,
    full_layout: bytes::Bytes,
    size: u64,
    refs: Vec<BlockRefOp>,
) -> Result<(bool, u64, OwnerVerdict, Vec<BlockRef>)> {
    match owner_of(be, ino)? {
        None => {
            note_local();
            // Rung 17: the AUTHORITY's own publishes on an ino with live
            // foreign custody must ALSO chain onto the head — its RAM
            // provenance goes stale under every served shipped merge, and
            // the un-chained gate would refuse (then full-Put-clobber the
            // peers' blocks). One relaxed probe on every solo mount
            // (`custody_owner()` is None).
            // Finding 35: the chain decision is STICKY like the serve
            // window (dlm::ino_has_range_custody carries the episode
            // latch) — a lapse-window unchained local merge full-Put
            // re-bases with a private view, forking the ino's chain.
            let granted = crate::data_grant::custody_owner()
                .map(|o| o.ino_granted(ino) || crate::dlm::ino_has_range_custody(ino))
                .unwrap_or(false);
            let _serve_window = local_publish_guard(ino).await;
            if granted {
                // Finding 36b (the AUTHORITY-LOCAL arm): the local
                // chained merge's recompute owns its released set too —
                // a displaced block a CO-WRITER minted is untracked on
                // this allocator, so the caller-frame local free refuses
                // it unseeded and it leaks (the fleet's steady refusal
                // stream). The released set travels UP (never freed
                // inline — the caller may hold the 3.5 stripe, RES-1)
                // and the save's post-guard venue runs the shipped-free
                // ladder over it.
                let (used, version, released) = be
                    .merge_layout_and_size_chained_accounted(ino, delta, full_layout, size, refs)
                    .await?;
                let verdict = OwnerVerdict {
                    recomputed: released.is_some(),
                    freed: Vec::new(),
                };
                Ok((used, version, verdict, released.unwrap_or_default()))
            } else {
                let used = be
                    .merge_layout_and_size(ino, delta, full_layout, size, refs)
                    .await?;
                Ok((
                    used,
                    if used { delta.version } else { 0 },
                    OwnerVerdict::default(),
                    Vec::new(),
                ))
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
                // The OWNER freed its recompute's releases (finding 36):
                // nothing travels back for the caller to FREE — what
                // travels is which offsets it freed (schema 15), the
                // caller's local-hygiene input.
                PublishReply::DeltaUsed {
                    used,
                    version,
                    recomputed,
                    freed,
                } => Ok((
                    used,
                    version,
                    OwnerVerdict { recomputed, freed },
                    Vec::new(),
                )),
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

/// Routed [`RoutedMetaBackend::migrate_block_map_train`] — the kvmap
/// crossing (PB-class files PR 2). The local arm runs the train on this
/// node under the serve-window discipline every local layout publish
/// keeps; the foreign-home arm ships the WHOLE map as the witnessed
/// [`PublishCall::MigrateBlockMap`] verb and the owner runs the train.
///
/// A local `Ok(None)` from the train (the tree could not engage) comes
/// back as a loud error rather than a silent local fallback: the CALLER
/// probed engagement before consuming anything, so this arm firing means
/// the ratchet failed mid-save — the never-lossy refill + retry owns it.
///
/// The second element is the SHIPPED arm's owner-freed set (schema 15 —
/// the blocks the owner's recompute ladder free-listed, the caller's
/// local-hygiene input); the local arm's released set stays inside the
/// outcome (`released` / `released_keys`) for the caller's own ladder, so
/// it answers an empty list here.
pub async fn migrate_block_map(
    be: &Arc<RoutedMetaBackend>,
    ino: Ino,
    layout: &[u8],
    size: u64,
    entries: Vec<(u32, String)>,
    refs: Vec<BlockRefOp>,
    chunk: usize,
    cursor_floor: u32,
    ref_for: &(dyn Fn(&str, u32) -> Option<crate::meta_backend::kv::block_refs::BlockRef>
          + Send
          + Sync),
    // The routing save's own LOCAL claims, `served: false`: PR 6c-i's
    // OVERLAY claims (take = dirty bindings, release = tombstones) for a
    // partial ino, or the finding-46 publish WINDOW (take = the window's
    // indices, `window: true`) for a whole-map ino's steady-state save.
    // The local arm runs them through the claims-scoped train (bounded
    // by pre-fix a); the shipped arm ignores them (the wire carries
    // entries + the refs frame, and the owner derives its claims there).
    local_claims: Option<crate::meta_backend::kv::backend::MapTrainClaims>,
) -> Result<(
    crate::meta_backend::kv::backend::MapMigrateOutcome,
    Vec<WireFreedBlock>,
)> {
    match owner_of(be, ino)? {
        None => {
            note_local();
            use crate::meta_backend::Metadata as _;
            let _serve_window = local_publish_guard(ino).await;
            // §11 row 4 / item 4 (f34 parity, lifted for kvmap bases): a
            // LOCAL whole-map train under live range grants reverts
            // peers' entries by delete-by-absence exactly like the
            // shipped one. The exemptions: the routing save's
            // episode-compose window (finding 35b — its train input IS
            // the durable head plus this save's claims, composed under
            // the same stripe the served scoped Puts serialize on, so
            // whole-map is exact there), and — since PR 5b — a STICKY
            // kvmap base outside the window, which runs CLAIMS-SCOPED
            // (adopt under this save's take claims, delete under its
            // release-without-take claims; the compose IS the
            // range-custody path now). A NON-kvmap base (a new crossing)
            // keeps the refusal: flipping the head mid-episode takes
            // whole-map authority nobody arbitrated — the caller's
            // crossing gate stands down to the blob arm instead.
            let live_grants = !serve_window_already_held()
                && crate::data_grant::custody_owner().is_some_and(|o| o.ino_has_range_grants(ino));
            let claims = if let Some(lc) = local_claims {
                // The overlay save (PR 6c-i) / the window save (finding
                // 46): the caller's claims ARE its own transitions —
                // authoritative under the held 4a, no frame derivation
                // needed (and the base is a sticky kvmap head by
                // construction).
                Some(lc)
            } else if live_grants {
                let kvmap_base = match be.getxattr(ino, "layout").await {
                    Ok(Some(bytes)) => crate::layout_wire::decode_layout_any(&bytes)
                        .ok()
                        .and_then(|l| l.block_map_id)
                        .is_some_and(|id| {
                            id.starts_with(crate::meta_backend::kv::block_map::KVMAP_HEAD_PREFIX)
                        }),
                    _ => false,
                };
                if !kvmap_base {
                    MAP_REFUSED.fetch_add(1, Ordering::Relaxed);
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "kvmap crossing for ino {ino} refused — the ino has live range \
                         grants, and a LOCAL whole-map crossing train would revert peers' \
                         entries (finding 34's class); the crossing stands down to the \
                         blob arm until range custody drains (map_refused)"
                    )));
                }
                let mut take: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
                let mut release: std::collections::BTreeSet<u32> =
                    std::collections::BTreeSet::new();
                for op in &refs {
                    if op.reference.is_map_blob() || op.reference.owner_ino != ino {
                        continue;
                    }
                    if op.take {
                        take.insert(op.reference.block_index);
                    } else {
                        release.insert(op.reference.block_index);
                    }
                }
                Some(crate::meta_backend::kv::backend::MapTrainClaims {
                    // The local authority composes against the head it
                    // serializes on — no staleness window for the belt.
                    base_gen: None,
                    take,
                    release,
                    // A LOCAL train (§14 S2 pre-fix b): custody arming —
                    // live here by the `live_grants` gate — is what
                    // mints, never claims-presence. Not an overlay save:
                    // the 5b recompute posture (global resolver) governs.
                    served: false,
                    overlay: false,
                    window: false,
                })
            } else {
                None
            };
            match be
                .migrate_block_map_train(
                    ino,
                    layout,
                    size,
                    &refs,
                    &entries,
                    chunk,
                    claims.as_ref(),
                    cursor_floor,
                    ref_for,
                )
                .await?
            {
                Some(outcome) => Ok((outcome, Vec::new())),
                None => Err(SqueezefsError::InvalidOperation(format!(
                    "kvmap crossing for ino {ino}: the block-map tree could not engage \
                     (the bit-16 ratchet failed) — nothing was committed; the caller's \
                     never-lossy ladder re-publishes"
                ))),
            }
        }
        Some(peer) => {
            intent_barrier_inos(&[ino]).await?;
            // PR 5b (design §11's belt): the shipped head's own kvmap id
            // carries the generation this ship was computed against — the
            // verb's base_gen is its explicit face (0 on a first
            // crossing, whose head carries no minted generation).
            let base_gen = crate::layout_wire::decode_layout_any(layout)
                .ok()
                .and_then(|l| l.block_map_id)
                .and_then(|id| crate::meta_backend::kv::block_map::parse_kvmap_head(&id).ok())
                .map(|h| h.gen)
                .unwrap_or(0);
            let call = PublishCall::MigrateBlockMap {
                ino,
                layout: layout.to_vec(),
                size,
                entries,
                refs: wire_refs(&refs),
                base_gen,
                lease_epoch: current_lease_epoch(),
                request_id: crate::cowriter::next_ship_request_id(),
            };
            match ship_witnessed(&peer, call).await? {
                PublishReply::MapMigrated {
                    records,
                    record_bytes,
                    preexisting,
                    recomputed,
                    freed,
                    gen,
                } => {
                    MAP_SHIPPED.fetch_add(1, Ordering::Relaxed);
                    // The owner freed its own recompute-released set —
                    // nothing travels back for the caller to FREE; which
                    // offsets it freed does (schema 15), beside the outcome.
                    Ok((
                        crate::meta_backend::kv::backend::MapMigrateOutcome {
                            records,
                            record_bytes,
                            preexisting,
                            gen,
                            recomputed,
                            released: Vec::new(),
                            released_keys: Vec::new(),
                            // Shipped trains never barrier (a live cursor on
                            // the owner refuses retried-class instead).
                            sweep_cursor: None,
                            swept_records: 0,
                            swept_freed: Vec::new(),
                        },
                        freed,
                    ))
                }
                other => Err(protocol_error(
                    "migrate_block_map",
                    &format!("{other:?}"),
                    "the train's accounting",
                )),
            }
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

// ---------------------------------------------------------------------------
// The authority's per-client lane-free notice ledger (schema 16)
// ---------------------------------------------------------------------------

/// One client's ledger: the grants served to it (the ordering witness) and
/// the notices queued for it since its last reply frame, deduped on the
/// block (a later release of a re-minted lifetime supersedes an earlier,
/// undrained one — safe because the grant that re-minted it was a reply
/// build, which drained the earlier notice first), so the backlog is
/// bounded by the client's lane blocks, never by time.
#[derive(Default)]
struct ClientLaneLedger {
    grants: AtomicU64,
    notices: std::sync::Mutex<std::collections::HashMap<(u64, u64), u64>>,
}

static LANE_LEDGERS: Lazy<scc::HashMap<String, Arc<ClientLaneLedger>>> =
    Lazy::new(scc::HashMap::new);
static LANE_FREE_NOTICES_QUEUED: AtomicU64 = AtomicU64::new(0);
static LANE_FREE_NOTICES_SHIPPED: AtomicU64 = AtomicU64::new(0);

fn client_lane_ledger(client: &str) -> Arc<ClientLaneLedger> {
    if let Some(l) = LANE_LEDGERS.read_sync(client, |_, l| Arc::clone(l)) {
        return l;
    }
    match LANE_LEDGERS.entry_sync(client.to_string()) {
        scc::hash_map::Entry::Occupied(occ) => Arc::clone(occ.get()),
        scc::hash_map::Entry::Vacant(vac) => {
            let l = Arc::new(ClientLaneLedger::default());
            let _ = vac.insert_entry(Arc::clone(&l));
            l
        }
    }
}

/// The enrolled owner of the lane `block_idx` sits in — `None` for the
/// authority's own lane, an unassigned lane, or an unpartitioned era.
fn lane_owner_of(block_idx: u64) -> Option<String> {
    let assignment = crate::data_grant::custody_owner()?.lane_assignment()?;
    let lane = crate::data_alloc_lane::block_lane_of(block_idx, assignment.writers());
    if lane == 0 {
        return None;
    }
    assignment
        .co_writers()
        .get(usize::try_from(lane - 1).ok()?)
        .cloned()
}

/// **Queue lane-free notices** for the owners of the lanes `blocks` sit in
/// — BEFORE the ladder that frees them runs (the ordering the harvest's
/// carriage guarantee rests on: a reply built after this point carries
/// the notice, and no reply built before it can hand the block back). The
/// requester of a served publish is skipped: its own blocks travel on the
/// per-call `freed` set, so a notice would be a second delivery. `None`
/// requester = the authority's own publish (every block is someone
/// else's).
pub(crate) fn note_lane_frees(requester: Option<&str>, vol_tag: u64, blocks: &[u64]) {
    for &block_idx in blocks {
        let Some(owner) = lane_owner_of(block_idx) else {
            continue;
        };
        if requester == Some(owner.as_str()) {
            continue;
        }
        let ledger = client_lane_ledger(&owner);
        let after_grants = ledger.grants.load(Ordering::Acquire);
        let mut notices = ledger.notices.lock().unwrap_or_else(|p| p.into_inner());
        notices.insert((vol_tag, block_idx), after_grants);
        LANE_FREE_NOTICES_QUEUED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Drain the notices queued for `client` into its reply frame.
fn take_lane_frees(client: &str) -> Vec<WireLaneFree> {
    let Some(ledger) = LANE_LEDGERS.read_sync(client, |_, l| Arc::clone(l)) else {
        return Vec::new();
    };
    let drained: Vec<WireLaneFree> = {
        let mut notices = ledger.notices.lock().unwrap_or_else(|p| p.into_inner());
        notices
            .drain()
            .map(|((vol_tag, block_idx), after_grants)| WireLaneFree {
                vol_tag,
                block_idx,
                after_grants,
            })
            .collect()
    };
    LANE_FREE_NOTICES_SHIPPED.fetch_add(drained.len() as u64, Ordering::Relaxed);
    drained
}

/// The next grant sequence for `client` — bumped once per served harvest,
/// strictly after its executor handed the blocks out.
fn next_grant_seq(client: &str) -> u64 {
    client_lane_ledger(client)
        .grants
        .fetch_add(1, Ordering::AcqRel)
        + 1
}

/// The owner-side lane-free HARVEST executor (rung 10, residual 2):
/// `(vol_tag, lane, writers, max, lease_epoch)` → the handed-out block
/// indices, each with its release age (ms on the authority's list since
/// its grace release — the lane-visible ledger's `released_served` stage,
/// finding 15 term 2; [`crate::free_grace::LANE_RELEASE_AGE_UNPLACED`] =
/// no mark) — [`crate::cowriter::execute_lane_harvest_aged`] over the
/// authority's data-plane router.
///
/// Installed by the multi-writer AUTHORITY arm beside the free executor —
/// the two are halves of one rewrite economy: the free RETURNS a co-writer's
/// displaced offset to the lane's supply, the harvest is what makes that
/// supply REACHABLE again.
pub type HarvestExecutor = Arc<
    dyn Fn(u64, u16, u16, u64, u64) -> Pin<Box<dyn Future<Output = Result<Vec<(u64, u64)>>> + Send>>
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

/// The owner-side BINDING WITNESS (finding 51,
/// `.benchmarks/2026-09-07-read-settle-lost-serialized-authority.md`): the
/// data-plane reaction to a SERVED layout commit — called with the DATA
/// references the commit TOOK, after the commit landed. The authority's
/// router publishes its incarnation word for every foreign-lane block
/// among them ([`crate::routing::BackendRouter::witness_served_bindings`]):
/// a co-writer publishes strictly after its DMA, so the serve is the one
/// event on the authority that witnesses the peer's device write behind a
/// key the authority can never mint — and without it the authority's word
/// for a RECYCLED co-writer block (retired by its own `begin_free` of the
/// previous lifetime) stayed unstable for ever, failing every later fill
/// of the key (the s11-mpiio row's `read_settle_lost_serialized` storm and
/// fsync EIOs).
///
/// Installed by the multi-writer AUTHORITY arm beside the free executor —
/// the free RETIRES a displaced offset's word, the witness RE-PUBLISHES it
/// when a peer's publish adopts the offset again: the two halves of one
/// lifetime, both on the authority's own data plane. Absent = no data
/// plane wired (solo mounts serve no publishes; a test rig without one
/// keeps the pre-f51 words).
pub type BindingWitness = Arc<dyn Fn(&[BlockRef]) + Send + Sync>;

static BINDING_WITNESS: Lazy<arc_swap::ArcSwapOption<BindingWitness>> =
    Lazy::new(arc_swap::ArcSwapOption::empty);

/// Install the process's served-binding witness (the authority arm's act).
pub fn install_binding_witness(witness: BindingWitness) {
    BINDING_WITNESS.store(Some(Arc::new(witness)));
}

/// Uninstall it (disarm / unmount / test teardown).
pub fn uninstall_binding_witness() {
    BINDING_WITNESS.store(None);
}

/// The DATA references a refs frame TAKES (map-blob custody excluded — a
/// blob is this authority's own lifecycle, never a peer's DMA).
fn taken_data_refs(refs: &[BlockRefOp]) -> Vec<BlockRef> {
    refs.iter()
        .filter(|o| o.take && !o.reference.is_map_blob())
        .map(|o| o.reference)
        .collect()
}

/// Hand a committed serve's taken data references to the installed
/// witness. Called strictly AFTER the commit landed: a refused or failed
/// serve adopted nothing, so it witnesses nothing.
fn witness_taken(taken: &[BlockRef]) {
    if taken.is_empty() {
        return;
    }
    if let Some(w) = BINDING_WITNESS.load_full() {
        (*w)(taken);
    }
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
/// direct merge, and the superseded key's take dangled (\[C8\] '1 durable
/// vs 0 layout references' + \[C2\] leak). The mount arm installs the
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

/// Finding 51, phase B1 (`.benchmarks/2026-09-07-read-settle-lost-
/// serialized-authority.md` §8) — the SERVED-publish half of
/// design-overlay-overwrite §5.7's one-authority screen: a served
/// layout commit that DISPLACES block `b`'s binding is a foreign durable
/// `Merge` on that index, and the authority's own device-overlay record
/// on `(ino, b)` (the assembler's shipped-slice shape) captured the key
/// the recompute is about to free. The routing primitive's hook never
/// sees a served commit (it lands on the backend directly), so the serve
/// wrapper hands the displaced indices to this sink — installed by the
/// authority fs beside the rung-18 invalidation — which invalidates the
/// fs's RAM head and supersedes the records (the durable map is the
/// authority the moment the commit lands; kept alive, the record's
/// settle would read a dead lifetime for ever — the B1 row's 28,039
/// `STALE BLOCK-KEY BINDING` refusals). Called strictly AFTER the commit
/// landed, before the recompute's frees run.
pub type ServedDisplacementSink = Arc<dyn Fn(u64, &[u32]) + Send + Sync>;

static SERVED_DISPLACEMENT_SINK: Lazy<arc_swap::ArcSwapOption<ServedDisplacementSink>> =
    Lazy::new(arc_swap::ArcSwapOption::empty);

/// Install the authority fs's served-displacement screen (the mount arm's
/// act, beside the invalidation sink).
pub fn install_served_displacement_sink(sink: ServedDisplacementSink) {
    SERVED_DISPLACEMENT_SINK.store(Some(Arc::new(sink)));
}

/// Uninstall it (disarm / unmount / test teardown).
pub fn uninstall_served_displacement_sink() {
    SERVED_DISPLACEMENT_SINK.store(None);
}

/// The map indices of `ino` a refs frame RELEASES (map-blob custody
/// excluded) — the bindings the commit displaced.
fn displaced_indices(ino: u64, refs: &[BlockRefOp]) -> Vec<u32> {
    let mut out: Vec<u32> = refs
        .iter()
        .filter(|o| !o.take && !o.reference.is_map_blob() && o.reference.owner_ino == ino)
        .map(|o| o.reference.block_index)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// [`displaced_indices`] over a recompute's released set.
fn displaced_indices_of(ino: u64, released: &[BlockRef]) -> Vec<u32> {
    let mut out: Vec<u32> = released
        .iter()
        .filter(|r| !r.is_map_blob() && r.owner_ino == ino)
        .map(|r| r.block_index)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Hand a committed serve's displaced indices to the installed screen.
fn note_served_displacements(ino: u64, indices: &[u32]) {
    if indices.is_empty() {
        return;
    }
    if let Some(sink) = SERVED_DISPLACEMENT_SINK.load_full() {
        sink(ino, indices);
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
/// Returns the grant: the handed-out block indices, the authority's
/// bound-age hint in ms (schema 8's OQ 2 field — 0 = the authority's ring
/// holds nothing; the caller falls back to its derivation) and, since
/// schema 14, each block's release age on the authority's list.
pub async fn ship_harvest_lane_free(
    endpoint: &str,
    vol_tag: u64,
    lane: u16,
    writers: u16,
    max: u64,
    lease_epoch: u64,
    request_id: u64,
) -> Result<LaneFreeGrant> {
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
            release_ages_ms,
            grant_seq,
        } => {
            HARVEST_SHIPPED_BLOCKS.fetch_add(blocks.len() as u64, Ordering::Relaxed);
            Ok(LaneFreeGrant {
                blocks,
                bound_age_ms,
                release_ages_ms,
                grant_seq,
            })
        }
        other => Err(protocol_error(
            "harvest_lane_free",
            &format!("{other:?}"),
            "a lane free grant",
        )),
    }
}

/// A shipped lane-free harvest's answer ([`ship_harvest_lane_free`]): the
/// [`PublishReply::LaneFreeGrant`] fields, owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneFreeGrant {
    /// The handed-out block indices of the caller's lane.
    pub blocks: Vec<u64>,
    /// The authority's live `free_grace_bound_age_ms` (0 = nothing held).
    pub bound_age_ms: u64,
    /// Per block in `blocks` order: ms on the authority's free list since
    /// its grace release ([`crate::free_grace::LANE_RELEASE_AGE_UNPLACED`]
    /// = no mark).
    pub release_ages_ms: Vec<u64>,
    /// The authority's per-client grant sequence this grant was served
    /// under (schema 16) — the tag every adopted block carries so a later
    /// lane-free notice with a lower `after_grants` is recognised as naming
    /// the offset's PREVIOUS lifetime.
    pub grant_seq: u64,
}

/// Count `blocks` abandoned shipped frees (`free_ship_failures` — the
/// leak-safe direction, loud). The one caller is
/// [`crate::cowriter::ship_displaced_frees`]'s abandon arm.
pub(crate) fn note_free_ship_failure(blocks: u64) {
    FREE_SHIP_FAILURES.fetch_add(blocks, Ordering::Relaxed);
}

/// Finding 36 (half 1) — run a served merge's RECOMPUTE-RELEASED data
/// blocks through the authority's own free ladder, strictly AFTER the
/// commit landed (the caller runs this only on Ok). Rides the installed
/// [`FreeExecutor`] — the `FreeBlocks` ladder verbatim, so the durable
/// population validation, the grace ring, S7's quarantine and the reclaim
/// manners compose unchanged, and a block still referenced elsewhere
/// answers `NonTerminal` instead of a wrongful device free. `Freed`
/// verdicts land on `free_recomputed_blocks` (the field engagement gauge)
/// and are RETURNED — the reply's `freed` set (schema 15): exactly the
/// blocks that entered this authority's free supply, which the shipper's
/// lane harvest can hand back, so the shipper retires its local tracking
/// of exactly those. `NonTerminal` (a clone sibling keeps the block alive)
/// and `Refused` blocks never reach a free list and do not travel. A
/// missing executor or a failed ladder is LEAK-SAFE and loud: the offsets
/// are durably unreferenced (the commit already released them), nothing
/// travels (nothing was free-listed), and the authority's next derivation
/// returns them.
///
/// `requester` is the served publish's client (`None` = the authority's
/// own publish — the assembler's fold, the episode compose). Every released
/// block in ANOTHER co-writer's lane is queued as a lane-free notice for
/// that lane's owner BEFORE the ladder runs ([`note_lane_frees`], schema
/// 16): the requester learns its own blocks from the returned `freed` set;
/// the lane owner of a block the requester's (or the authority's) publish
/// displaced has no reply to read it from, and its local tracking would
/// otherwise linger until the lane harvest handed the offset back — the
/// fleet's `block_claim_anomalies` population.
pub(crate) async fn free_recomputed_releases(
    ino: u64,
    released: Vec<BlockRef>,
    requester: Option<&str>,
) -> Vec<WireFreedBlock> {
    // Dedup on the durable identity: one transition can release the same
    // device block at two map indexes — its free runs once.
    let mut seen: std::collections::HashSet<(u64, u64)> = std::collections::HashSet::new();
    let mut by_vol: std::collections::BTreeMap<u64, Vec<u64>> = std::collections::BTreeMap::new();
    for r in released {
        if !r.is_map_blob() && seen.insert((r.vol_tag, r.block_idx)) {
            by_vol.entry(r.vol_tag).or_default().push(r.block_idx);
        }
    }
    let mut freed_blocks: Vec<WireFreedBlock> = Vec::new();
    if by_vol.is_empty() {
        return freed_blocks;
    }
    for (vol_tag, blocks) in &by_vol {
        note_lane_frees(requester, *vol_tag, blocks);
    }
    let Some(exec) = free_executor() else {
        log::error!(
            "S9 (finding 36): a served merge for ino {ino} recomputed {} released block(s) but \
             no free executor is installed — the offsets stay durably unreferenced until the \
             authority's next derivation (leak-safe, loud)",
            seen.len()
        );
        return freed_blocks;
    };
    for (vol_tag, blocks) in by_vol {
        let count = blocks.len();
        match exec(vol_tag, blocks.clone()).await {
            Ok(verdicts) => {
                let before = freed_blocks.len();
                freed_blocks.extend(
                    blocks
                        .iter()
                        .zip(verdicts.iter())
                        .filter(|(_, v)| **v == FreeVerdict::Freed)
                        .map(|(block_idx, _)| WireFreedBlock {
                            vol_tag,
                            block_idx: *block_idx,
                        }),
                );
                FREE_RECOMPUTED_BLOCKS
                    .fetch_add((freed_blocks.len() - before) as u64, Ordering::Relaxed);
            }
            Err(e) => {
                log::error!(
                    "S9 (finding 36): the recomputed-release free ladder failed for {count} \
                     block(s) on vol_tag {vol_tag:#016x} (ino {ino}): {e} — leak-safe (the \
                     commit already released them durably; the next derivation returns them)"
                );
            }
        }
    }
    freed_blocks
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
/// the owner-partitioned half of
/// [`crate::cowriter::durable_block_refcounts_with`] (finding 13): the
/// answer is the peer's LIVE-tree count over the volumes it owns, in
/// request order.
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

/// The custody-scoped compose's verdict (`custody_scoped_layout`): the
/// layout to stage, the recomputed accounting (`None` on every verbatim
/// arm), the caller frame's RAM-only lifetimes (finding 15 — freed with the
/// released set, staged nowhere) and the compose's blob custody.
struct ScopedLayout {
    layout: Vec<u8>,
    recomputed: Option<Vec<BlockRefOp>>,
    ram_only_releases: Vec<crate::meta_backend::kv::block_refs::BlockRef>,
    blob_custody: ScopedBlobCustody,
}

/// What a SetLayoutAndSize serve's commit outcome must settle (D-1c — the
/// FINISH half's input; see [`PublishService::finish_layout_publish`]).
struct LayoutPostCommit {
    blob_custody: ScopedBlobCustody,
    /// Finding 36b: the scoped compose's released DATA blocks — freed
    /// through this authority's own ladder strictly after commit Ok.
    released_data: Vec<BlockRef>,
    /// Finding 51: the DATA blocks the commit TAKES — handed to the binding
    /// witness strictly after commit Ok (the peer's DMA behind each is
    /// complete; the serve is the authority's witness of it).
    taken_data: Vec<BlockRef>,
    /// Finding 51, phase B1: the map indices the commit DISPLACED —
    /// handed to the served-displacement screen strictly after commit Ok,
    /// before the recompute frees their old bindings.
    displaced: Vec<u32>,
    was_recomputed: bool,
}

/// A SetLayoutAndSize serve past its PREPARE half: the staged commit item
/// (a [`RoutedMetaBackend::set_layout_and_size_group`] member) and its
/// post-commit settlement.
struct PreparedLayoutPublish {
    item: LayoutPublish,
    post: LayoutPostCommit,
}

/// The PREPARE half's verdict: the call completed inside the prepare (the
/// kvmap scoped-put train committed it — never groupable), or it is
/// staged for the layout commit.
enum LayoutPrepare {
    Done(PublishReply),
    Staged(PreparedLayoutPublish),
}

/// A witness slot the group serve CLAIMED (D-1c): this round owns the
/// `(lease_epoch, request_id)` execution, every concurrent
/// `serve_layout_publish` of the same key parks on the slot exactly as on
/// an async winner. Completed with the member's own outcome; dropped
/// un-completed (an unwind before the answer) it ABANDONS the claim so a
/// parked replay re-elects — the cancelled-leader shape `sqz_once` gives
/// an async leader, made explicit.
struct WitnessLease {
    slot: Arc<squeezefs_ipc::sqz_once::OnceCell<std::result::Result<PublishReply, WireError>>>,
    completed: bool,
}

impl WitnessLease {
    fn complete(mut self, outcome: std::result::Result<PublishReply, WireError>) {
        self.slot.complete_init(outcome);
        self.completed = true;
    }
}

impl Drop for WitnessLease {
    fn drop(&mut self) {
        if !self.completed {
            self.slot.abandon_init();
        }
    }
}

/// One claimed member of a round's layout group: its frame slot, the
/// call, and the witness it owns.
struct GroupMember {
    idx: usize,
    call: PublishCall,
    lease: WitnessLease,
}

/// PR 5b items 3+4 — a kvmap-headed serve's custody verdict for its
/// CLAIM SCOPE (see [`PublishService::kvmap_claim_scope`]).
enum KvmapClaimScope {
    /// Claims apply span-unfiltered (the §11 law-b baseline).
    Unscoped,
    /// A RANGE holder: claims filter to its spans minus demoted regions.
    Scoped {
        spans: Vec<(u64, u64)>,
        demoted: Vec<(u64, u64)>,
        block: u64,
    },
    /// Finding 34's class: a custody-less writer against OTHER holders'
    /// live grants — the caller refuses on its own counter.
    UnscopedAgainstGrants,
}

/// The publish path executed for a peer, against the volumes this node has
/// authority over.
///
/// The **venue** is S8's, verbatim (`meta_ship::owner_dispatch`, D-5): the
/// frame arrives on the connection's own thread and every call / group /
/// free / harvest dispatch goes through the one door that records the
/// dispatch-hop split and selects the venue — the accepting thread itself
/// by default, the shipped `sqz-meta` hop under
/// `SQUEEZEFS_META_SHIP_INLINE_SERVE=0`. The hop's mechanical reason (the
/// conveyor pass task once spawned on the committer's AMBIENT runtime) is
/// gone since rip-tokio-total: every task a served commit touches lives on
/// an explicit process-global venue.
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
    /// one window would make their id spaces collide). The cached outcome
    /// is the aged block list plus the grant sequence it was served under
    /// (schema 16), so a replay re-answers the same sequence.
    harvest_dedup: DedupWindow<std::result::Result<(Vec<(u64, u64)>, u64), WireError>>,
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

    /// A per-call refusal (nothing applied), logged loud.
    fn refuse(status: u16, reason: String) -> PublishCallOutcome {
        log::warn!("S9 publish owner refused a call: {reason}");
        PublishCallOutcome::Refused {
            status,
            detail: reason,
        }
    }

    /// A frame-level refusal — the wire status covers only what no call
    /// can own: an unknown verb, an undecodable frame, a schema mismatch.
    fn refuse_frame(id: u64, status: u16, reason: String) -> RpcResponse {
        log::warn!("S9 publish owner refused a frame: {reason}");
        RpcResponse {
            id,
            status,
            body: reason.into_bytes(),
        }
    }

    /// Serve one publish FRAME (D-1b): decode, screen the schema, then
    /// partition its calls into dependency chains by named-inode overlap
    /// (D-1's relation, [`super::service::chains_by_named_inos`]) —
    /// calls naming a common inode execute serially in submission order,
    /// distinct chains concurrently — so a frame's independent publishes
    /// park on the M7 conveyor together and co-queue into one pass.
    /// Outcomes are re-slotted into call order. Every per-call law (the
    /// not-owner screen, the era gate, the witness window, the lane/free
    /// validations, the unwind record) runs inside [`Self::serve_call`].
    ///
    /// **D-1c — one conveyor group per shipped frame** (e2e perf audit
    /// §5.3 row 1): since C-2's instant-drain apply pass, "co-queue" was a
    /// venue ratio — a frame's calls reach `commit_tx` at different
    /// instants (each prelude awaits), so 24 publishes landed in 3–14
    /// passes. With the lever on ([`CONVEYOR_GROUP_ENV`], default), the
    /// multi-chain dispatch proceeds in ROUNDS: every chain contributes
    /// its next call; the round's `SetLayoutAndSize` calls are PREPARED
    /// concurrently and committed as ONE
    /// [`RoutedMetaBackend::set_layout_and_size_group`] (one queue-lock
    /// enqueue on the conveyor — one apply pass by construction) while
    /// the round's other calls serve concurrently as before; a chain's
    /// next call starts only after its previous one finished (the chain
    /// order law), so a 24-distinct-ino frame is 24 chains of length 1 →
    /// one round → one group → one pass. `=0` keeps the D-1b per-chain
    /// dispatch verbatim (the A/B control).
    ///
    /// The venue rule (S8's, verbatim): a call's execution hops to the
    /// sqz-meta pool inside `serve_call` exactly as it did per frame; the
    /// chain futures themselves only sequence and await those hops, so a
    /// one-call frame costs the one hop it always cost. A round's group
    /// is ONE hop for all its members.
    async fn serve(&self, req: RpcRequest) -> RpcResponse {
        if req.verb != VERB_PUBLISH_CALL {
            return RpcResponse {
                id: req.id,
                status: RPC_UNKNOWN_VERB,
                body: format!("S9: unknown publish verb {}", req.verb).into_bytes(),
            };
        }
        let frame = match decode_request_frame(&req.body) {
            Ok(f) => f,
            Err(e) => return Self::refuse_frame(req.id, PUBLISH_MALFORMED, format!("{e}")),
        };
        if frame.schema != PUBLISH_SCHEMA {
            return Self::refuse_frame(
                req.id,
                PUBLISH_SCHEMA_MISMATCH,
                format!(
                    "peer speaks publish schema {} and this owner speaks {PUBLISH_SCHEMA} — \
                     refusing rather than guessing at a layout-bearing frame",
                    frame.schema
                ),
            );
        }
        let n = frame.calls.len();
        if n == 0 {
            return Self::refuse_frame(
                req.id,
                PUBLISH_MALFORMED,
                "a publish frame carries at least one call".into(),
            );
        }
        SERVED_FRAMES.fetch_add(1, Ordering::Relaxed);
        SERVED_FRAME_CALLS.fetch_add(n as u64, Ordering::Relaxed);
        let client: Arc<str> = Arc::from(frame.client.as_str());
        let named: Vec<Vec<u64>> = frame.calls.iter().map(PublishCall::named_inos).collect();
        let chains = super::service::chains_by_named_inos(&named);
        let chain_count = chains.iter().copied().max().map_or(0, |m| m + 1);
        SERVED_CHAINS.fetch_add(chain_count as u64, Ordering::Relaxed);
        // D-5: the dispatch venue, read once per frame (see
        // `super::owner_dispatch`).
        let inline = super::inline_serve_enabled();
        let outcomes: Vec<PublishCallOutcome> = if chain_count <= 1 {
            // One chain (the one-call frame, or one hot object): the
            // serial form, in-task.
            let mut out = Vec::with_capacity(n);
            for call in frame.calls {
                out.push(self.serve_call(&client, call, inline).await);
            }
            out
        } else {
            let mut per_chain: Vec<std::collections::VecDeque<(usize, PublishCall)>> =
                vec![std::collections::VecDeque::new(); chain_count];
            for (idx, (call, chain)) in frame.calls.into_iter().zip(chains).enumerate() {
                per_chain[chain].push_back((idx, call));
            }
            let mut slots: Vec<Option<PublishCallOutcome>> = (0..n).map(|_| None).collect();
            if conveyor_group_enabled() {
                // D-1c rounds (see the method doc).
                let mut grouped = false;
                loop {
                    let round: Vec<(usize, PublishCall)> =
                        per_chain.iter_mut().filter_map(|c| c.pop_front()).collect();
                    if round.is_empty() {
                        break;
                    }
                    let (layouts, others): (Vec<_>, Vec<_>) = round
                        .into_iter()
                        .partition(|(_, c)| matches!(c, PublishCall::SetLayoutAndSize { .. }));
                    let others_fut =
                        futures::future::join_all(others.into_iter().map(|(idx, call)| {
                            let client = Arc::clone(&client);
                            async move { (idx, self.serve_call(&client, call, inline).await) }
                        }));
                    let (others_out, (group_out, committed)) = futures::join!(
                        others_fut,
                        self.serve_layout_group(&client, layouts, inline)
                    );
                    grouped |= committed;
                    for (idx, outcome) in others_out.into_iter().chain(group_out) {
                        slots[idx] = Some(outcome);
                    }
                }
                if grouped {
                    FRAME_GROUPS.fetch_add(1, Ordering::Relaxed);
                }
            } else {
                // The D-1b per-chain dispatch — the lever's control.
                let futs = per_chain.into_iter().map(|chain| {
                    let client = Arc::clone(&client);
                    async move {
                        let mut out = Vec::with_capacity(chain.len());
                        for (idx, call) in chain {
                            out.push((idx, self.serve_call(&client, call, inline).await));
                        }
                        out
                    }
                });
                for chain_out in futures::future::join_all(futs).await {
                    for (idx, outcome) in chain_out {
                        slots[idx] = Some(outcome);
                    }
                }
            }
            slots
                .into_iter()
                .map(|slot| {
                    // Every call was slotted into exactly one chain; the
                    // arm below is unreachable and refuses loud if not.
                    slot.unwrap_or_else(|| {
                        Self::refuse(
                            PUBLISH_MALFORMED,
                            "S9 publish owner: a framed call was dispatched to no chain".into(),
                        )
                    })
                })
                .collect()
        };
        // Schema 16: the frame's client takes every lane-free notice queued
        // for it since its last reply — drained AFTER the calls executed,
        // so a harvest in this frame that handed an offset back finds the
        // notice its free queued before the ladder ran.
        let reply = PublishReplyFrame {
            schema: PUBLISH_SCHEMA,
            outcomes,
            lane_frees: take_lane_frees(&client),
        };
        match encode_reply_frame(&reply) {
            Ok(body) => RpcResponse {
                id: req.id,
                status: PUBLISH_OK,
                body,
            },
            Err(e) => Self::refuse_frame(req.id, PUBLISH_MALFORMED, format!("reply encode: {e}")),
        }
    }

    /// The two gates every call passes first, in their landed order: the
    /// not-owner screen, then the ERA gate. `Some` = the call's refusal
    /// (nothing applied). Shared by [`Self::serve_call`] and the D-1c
    /// group serve so a grouped call is gated exactly as a serial one.
    fn screen(&self, client: &str, call: &PublishCall) -> Option<PublishCallOutcome> {
        if let Some(foreign) = call
            .named_inos()
            .into_iter()
            .find(|ino| !self.has_authority(*ino))
        {
            NOT_OWNER.fetch_add(1, Ordering::Relaxed);
            return Some(Self::refuse(
                PUBLISH_NOT_OWNER,
                format!(
                    "ino {foreign} routes to a metadata volume this node holds no authority \
                     over — the client's ownership map is stale (re-read the volumes' \
                     writer_claim records)"
                ),
            ));
        }
        // Finding #6 (design-mw-layout-versions §6a, law 1): the ERA GATE on
        // every schema-5 mutating verb — a swept-but-not-yet-self-fenced
        // zombie's publishes REFUSE here, and a refusal means NOTHING was
        // applied. Refused BEFORE the witness window on purpose (a dead
        // era's replay must never be answered from cache — the FreeBlocks
        // precedent verbatim). The raise/free/harvest verbs keep their own,
        // older gates in `serve_call` (landed counter surface).
        if let Some(epoch) = call.era_gated_epoch() {
            if let Err(reason) = crate::data_grant::validate_publish_era(client, epoch) {
                // Rung 17: the extent class's era refusals land on their
                // OWN row (`extent_stale_refusals`); the layout class
                // keeps the finding-#6 row.
                if call.is_extent() {
                    EXTENT_STALE_REFUSALS.fetch_add(1, Ordering::Relaxed);
                } else {
                    STALE_REFUSALS.fetch_add(1, Ordering::Relaxed);
                }
                return Some(Self::refuse(PUBLISH_STALE_LEASE, reason));
            }
        }
        None
    }

    /// Serve ONE call of a frame — the gates in their landed order, then
    /// the class's own serve path. Refusals are per call and apply
    /// nothing; a sibling call in the same frame is untouched. `inline` is
    /// the frame's dispatch venue (D-5, [`super::owner_dispatch`]).
    async fn serve_call(
        &self,
        client: &str,
        call: PublishCall,
        inline: bool,
    ) -> PublishCallOutcome {
        if let Some(refusal) = self.screen(client, &call) {
            return refusal;
        }
        // The layout-publish class (law 2) and rung 17's extent class:
        // witnessed — served through the dedup window, never through the
        // generic dispatch below, whose no-retry law it would otherwise
        // weaken.
        if let Some((epoch, request_id)) = call.witness() {
            return self
                .serve_layout_publish(epoch, request_id, client.to_string(), call, inline)
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
        } = &call
        {
            if let Err(reason) =
                crate::data_grant::validate_lane_raise(client, *lease_epoch, *lane, *writers)
            {
                crate::fuse_client::METRICS
                    .alloc_lane_raise_refusals
                    .fetch_add(1, Ordering::Relaxed);
                return Self::refuse(PUBLISH_LANE_REFUSED, reason);
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
        } = &call
        {
            if let Err(reason) =
                crate::data_grant::validate_lane_raise(client, *lease_epoch, *lane, *writers)
            {
                HARVEST_REFUSALS.fetch_add(1, Ordering::Relaxed);
                return Self::refuse(PUBLISH_LANE_REFUSED, reason);
            }
            return self
                .serve_harvest(client, *lease_epoch, *request_id, call, inline)
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
        } = &call
        {
            // The era gate FIRST, before the window: a dead era must be
            // refused whether or not its id once executed — answering a
            // dead era's replay from cache would tell a fenced mount its
            // custody still speaks.
            if let Err(reason) = crate::data_grant::validate_free(client, *lease_epoch) {
                FREE_STALE_REFUSALS.fetch_add(1, Ordering::Relaxed);
                return Self::refuse(PUBLISH_STALE_LEASE, reason);
            }
            return self
                .serve_free(client, *lease_epoch, *request_id, call, inline)
                .await;
        }
        let Some(me) = self.owned() else {
            return Self::refuse(
                PUBLISH_MALFORMED,
                "S9 publish service is shutting down — no handle to dispatch on".into(),
            );
        };
        let name = call.name();
        let client = client.to_string();
        let (joined, _) = super::owner_dispatch("meta_ship_publish_verb", inline, async move {
            me.execute(&client, call).await
        })
        .await;
        match joined {
            Ok(outcome) => {
                SERVED.fetch_add(1, Ordering::Relaxed);
                PublishCallOutcome::Done(outcome.map_err(|e| WireError::from_error(&e)))
            }
            Err(e) => {
                // An owner-side publish UNWOUND. Nothing joins a data-path
                // task, so this counter is the only record its work was
                // lost (the RES-7/RES-8 discipline).
                PANICS.fetch_add(1, Ordering::Relaxed);
                log::error!("S9 publish owner-side execution of {name} unwound: {e}");
                Self::refuse(
                    PUBLISH_PANIC,
                    format!("S9 publish owner-side execution panicked: {e}"),
                )
            }
        }
    }

    /// Serve one LAYOUT-PUBLISH call (`SetLayoutAndSize` /
    /// `MergeLayoutAndSize` / `CommitBlockRefs`) through the witness
    /// window (finding #6, design §6a law 2): the winner of
    /// `(lease_epoch, request_id)` executes on the sqz-meta pool; every
    /// duplicate — a freeze-window lost-reply retry, or an overlapping
    /// resend — awaits the winner's own outcome and is counted
    /// (`replays`). The era gate already ran in [`Self::serve_call`], so
    /// a dead era can never reach (or be answered from) this window.
    async fn serve_layout_publish(
        &self,
        lease_epoch: u64,
        request_id: u64,
        client: String,
        call: PublishCall,
        inline: bool,
    ) -> PublishCallOutcome {
        let Some(me) = self.owned() else {
            return Self::refuse(
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
                // The dispatch door (D-5): the commit runs on the venue the
                // lever selects, its unwind contained INSIDE the witness
                // init so a replay meets a cached outcome, never a claimed
                // slot nobody completes.
                let (joined, _) =
                    super::owner_dispatch("meta_ship_publish_verb", inline, async move {
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
                            Some(serve_ino_guard(ino).await)
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
                    .await;
                match joined {
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
        PublishCallOutcome::Done(outcome)
    }

    /// Serve a round's `SetLayoutAndSize` calls as ONE conveyor group
    /// (D-1c). Each call passes [`Self::screen`] exactly as a serial call
    /// does; each then CLAIMS its `(lease_epoch, request_id)` witness slot
    /// synchronously ([`WitnessLease`]) — a claim that fails means a live
    /// winner (or a settled outcome) exists, so that call takes the serial
    /// path, which parks on the winner and counts the replay, exactly as
    /// before. The claimed calls run [`Self::execute_layout_group`]; the
    /// serial ones run beside it. Returns the per-call outcomes and
    /// whether a group was committed (the `frame_groups` engagement).
    async fn serve_layout_group(
        &self,
        client: &Arc<str>,
        calls: Vec<(usize, PublishCall)>,
        inline: bool,
    ) -> (Vec<(usize, PublishCallOutcome)>, bool) {
        let mut out = Vec::with_capacity(calls.len());
        if calls.is_empty() {
            return (out, false);
        }
        let mut members: Vec<GroupMember> = Vec::with_capacity(calls.len());
        let mut serial: Vec<(usize, PublishCall)> = Vec::new();
        for (idx, call) in calls {
            if let Some(refusal) = self.screen(client, &call) {
                out.push((idx, refusal));
                continue;
            }
            let Some(key) = call.witness() else {
                serial.push((idx, call));
                continue;
            };
            let (slot, _) = self.publish_dedup.slot(key);
            if slot.claim_init() {
                members.push(GroupMember {
                    idx,
                    call,
                    lease: WitnessLease {
                        slot,
                        completed: false,
                    },
                });
            } else {
                serial.push((idx, call));
            }
        }
        let serial_fut = futures::future::join_all(serial.into_iter().map(|(idx, call)| {
            let client = Arc::clone(client);
            async move { (idx, self.serve_call(&client, call, inline).await) }
        }));
        let (serial_out, (group_out, committed)) = futures::join!(
            serial_fut,
            self.execute_layout_group(client, members, inline)
        );
        out.extend(serial_out);
        out.extend(group_out);
        (out, committed)
    }

    /// Execute the claimed members of a round as one group: ONE dispatch
    /// ([`super::owner_dispatch`] — the accepting venue by default, one hop
    /// onto the sqz-meta pool under the control) running
    /// [`Self::run_layout_group`]; every member's witness completes with
    /// its own outcome (a parked replay wakes with it), and an unwind is
    /// recorded AND cached per member — a replay answers the same loud
    /// failure instead of re-running half a commit (the
    /// `serve_layout_publish` law).
    async fn execute_layout_group(
        &self,
        client: &Arc<str>,
        members: Vec<GroupMember>,
        inline: bool,
    ) -> (Vec<(usize, PublishCallOutcome)>, bool) {
        if members.is_empty() {
            return (Vec::new(), false);
        }
        let mut out = Vec::with_capacity(members.len());
        let Some(me) = self.owned() else {
            // Shutting down: the leases drop ABANDONED (a parked replay
            // re-elects and meets its own refusal).
            for m in members {
                out.push((
                    m.idx,
                    Self::refuse(
                        PUBLISH_MALFORMED,
                        "S9 publish service is shutting down — no handle to dispatch on".into(),
                    ),
                ));
            }
            return (out, false);
        };
        let mut idxs = Vec::with_capacity(members.len());
        let mut leases = Vec::with_capacity(members.len());
        let mut work = Vec::with_capacity(members.len());
        for m in members {
            idxs.push(m.idx);
            leases.push(m.lease);
            work.push(m.call);
        }
        let client = client.to_string();
        let (joined, _) = super::owner_dispatch("meta_ship_publish_group", inline, async move {
            me.run_layout_group(&client, work).await
        })
        .await;
        match joined {
            Ok((outcomes, committed)) => {
                for ((idx, lease), outcome) in idxs.into_iter().zip(leases).zip(outcomes) {
                    lease.complete(outcome.clone());
                    SERVED.fetch_add(1, Ordering::Relaxed);
                    out.push((idx, PublishCallOutcome::Done(outcome)));
                }
                (out, committed)
            }
            Err(e) => {
                // RES-7/RES-8: the unwind is recorded — and CACHED per
                // member, so a replay answers the same loud failure.
                PANICS.fetch_add(1, Ordering::Relaxed);
                log::error!("S9 publish owner-side group execution unwound: {e}");
                let failure: std::result::Result<PublishReply, WireError> =
                    Err(WireError::from_error(&SqueezefsError::InvalidOperation(
                        format!("S9 publish owner-side execution panicked: {e}"),
                    )));
                for (idx, lease) in idxs.into_iter().zip(leases) {
                    lease.complete(failure.clone());
                    SERVED.fetch_add(1, Ordering::Relaxed);
                    out.push((idx, PublishCallOutcome::Done(failure.clone())));
                }
                (out, false)
            }
        }
    }

    /// The group's execution, ON the sqz-meta pool: the round's serve
    /// stripes (deduped by stripe index, ascending — `StripeLocks`'s
    /// multi-acquisition law; two distinct inos may share a stripe), then
    /// every member's PREPARE concurrently, then ONE
    /// [`RoutedMetaBackend::set_layout_and_size_group`] over the staged
    /// items (the backend takes the members' 4a guards as one canonical
    /// `lock_many` and enqueues the group under one queue lock), then
    /// every member's FINISH. Members whose prepare completed the call
    /// (the kvmap train) or failed never join the group. Per-member lock
    /// ORDER is the serial serve's verbatim — serve stripe → (chunk
    /// commits' own 4a) → 4a → commit — so the group adds no new edge
    /// class; what it adds is holding several stripes of each layer at
    /// once, which the ascending acquisition keeps acyclic against every
    /// other multi-acquirer (`lock_many`) and every single-stripe holder.
    /// Returns one outcome per member (input order) and whether a group
    /// was committed.
    async fn run_layout_group(
        &self,
        client: &str,
        work: Vec<PublishCall>,
    ) -> (Vec<std::result::Result<PublishReply, WireError>>, bool) {
        let n = work.len();
        let inos: Vec<u64> = work
            .iter()
            .map(|c| c.named_inos().first().copied().unwrap_or(0))
            .collect();
        // (stripe, ino): dedup by stripe keeps the FIRST member as the
        // stripe's census identity — an in-group collision is one acquire,
        // never a self-collision.
        let mut stripes: Vec<(usize, u64)> =
            inos.iter().map(|&i| (serve_ino_stripe(i), i)).collect();
        stripes.sort_unstable_by_key(|&(s, _)| s);
        stripes.dedup_by_key(|&mut (s, _)| s);
        let mut stripe_guards = Vec::with_capacity(stripes.len());
        for (s, ino) in stripes {
            stripe_guards.push(serve_ino_guard_by_index(s, ino).await);
        }
        let prepared = futures::future::join_all(work.into_iter().map(|call| async move {
            let PublishCall::SetLayoutAndSize {
                ino,
                layout,
                size,
                refs,
                ..
            } = call
            else {
                return Err(SqueezefsError::InvalidOperation(
                    "S9 publish owner: a non-layout call was dispatched to a layout group".into(),
                ));
            };
            self.prepare_layout_publish(client, ino, layout, size, refs)
                .await
        }))
        .await;
        let mut outcomes: Vec<Option<std::result::Result<PublishReply, WireError>>> =
            (0..n).map(|_| None).collect();
        let mut invalidate: Vec<u64> = Vec::with_capacity(n);
        let mut items = Vec::new();
        let mut posts = Vec::new();
        let mut item_slots = Vec::new();
        for (i, p) in prepared.into_iter().enumerate() {
            match p {
                Err(e) => outcomes[i] = Some(Err(WireError::from_error(&e))),
                Ok(LayoutPrepare::Done(reply)) => {
                    invalidate.push(inos[i]);
                    outcomes[i] = Some(Ok(reply));
                }
                Ok(LayoutPrepare::Staged(PreparedLayoutPublish { item, post })) => {
                    items.push(item);
                    posts.push(post);
                    item_slots.push(i);
                }
            }
        }
        let committed = !items.is_empty();
        if committed {
            let results = self.inner.set_layout_and_size_group(items).await;
            for ((i, post), committed) in item_slots.into_iter().zip(posts).zip(results) {
                let out = Self::finish_layout_publish(client, inos[i], committed, post).await;
                if out.is_ok() {
                    invalidate.push(inos[i]);
                }
                outcomes[i] = Some(out.map_err(|e| WireError::from_error(&e)));
            }
        }
        // Rung 18: a committed layout-class serve invalidates the
        // authority fs's RAM view of the named inos (the served commit
        // bypassed it — see the sink's doc). Under the stripes, as the
        // serial serve does.
        if !invalidate.is_empty() {
            note_served_layout_commit(&invalidate);
        }
        drop(stripe_guards);
        let outcomes = outcomes
            .into_iter()
            .map(|slot| {
                slot.unwrap_or_else(|| {
                    Err(WireError::from_error(&SqueezefsError::InvalidOperation(
                        "S9 publish owner: a grouped call reached no outcome (unreachable — \
                         every member is answered by its prepare or its commit)"
                            .into(),
                    )))
                })
            })
            .collect();
        (outcomes, committed)
    }

    /// Serve one [`PublishCall::FreeBlocks`] through the dedup window: the
    /// winner of `(lease_epoch, request_id)` executes on the sqz-meta
    /// pool under the installed [`FreeExecutor`]; every duplicate —
    /// a lost-reply retry, or an overlapping resend — awaits the winner's
    /// own outcome and is counted (`free_replays`). The era gate already
    /// ran in [`Self::serve_call`].
    async fn serve_free(
        &self,
        client: &str,
        lease_epoch: u64,
        request_id: u64,
        call: PublishCall,
        inline: bool,
    ) -> PublishCallOutcome {
        let PublishCall::FreeBlocks {
            vol_tag, blocks, ..
        } = call
        else {
            return Self::refuse(
                PUBLISH_MALFORMED,
                "serve_free dispatched a non-free call".into(),
            );
        };
        let Some(exec) = free_executor() else {
            return Self::refuse(
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
        if owns {
            // Schema 16: a block in ANOTHER co-writer's lane (a predecessor
            // that peer minted, which this shipper's rewrite displaced) is
            // noticed to its lane owner before the ladder frees it — the
            // shipper retires its own view on the verdicts it gets back.
            note_lane_frees(Some(client), vol_tag, &blocks);
        }
        let outcome = slot
            .get_or_init(|| async move {
                // The dispatch door (D-5): the ladder runs on the venue the
                // lever selects; its unwind is contained inside the witness
                // init (see `serve_layout_publish`).
                let (joined, _) =
                    super::owner_dispatch("shipped_free_ladder", inline, exec(vol_tag, blocks))
                        .await;
                match joined {
                    Ok(Ok(verdicts)) => {
                        FREE_SERVED_BLOCKS.fetch_add(
                            verdicts
                                .iter()
                                .filter(|v| **v == FreeVerdict::Freed)
                                .count() as u64,
                            Ordering::Relaxed,
                        );
                        FREE_REFUSED_BLOCKS.fetch_add(
                            verdicts
                                .iter()
                                .filter(|v| **v == FreeVerdict::Refused)
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
        PublishCallOutcome::Done(outcome.map(PublishReply::FreeVerdicts))
    }

    /// Serve one [`PublishCall::HarvestLaneFree`] through its dedup window
    /// — the [`Self::serve_free`] pattern verbatim: the winner of
    /// `(lease_epoch, request_id)` executes on the sqz-meta pool under the
    /// installed [`HarvestExecutor`]; every duplicate awaits the winner's
    /// own grant and is counted (`harvest_replays`). The lane + era gate
    /// already ran in [`Self::serve_call`].
    async fn serve_harvest(
        &self,
        client: &str,
        lease_epoch: u64,
        request_id: u64,
        call: PublishCall,
        inline: bool,
    ) -> PublishCallOutcome {
        let PublishCall::HarvestLaneFree {
            vol_tag,
            lane,
            writers,
            max,
            ..
        } = call
        else {
            return Self::refuse(
                PUBLISH_MALFORMED,
                "serve_harvest dispatched a non-harvest call".into(),
            );
        };
        let Some(exec) = harvest_executor() else {
            return Self::refuse(
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
        let client_owned = client.to_string();
        let outcome = slot
            .get_or_init(|| async move {
                // The dispatch door (D-5), as `serve_free`.
                let (joined, _) = super::owner_dispatch(
                    "shipped_lane_harvest",
                    inline,
                    exec(vol_tag, lane, writers, max, lease_epoch),
                )
                .await;
                match joined {
                    Ok(Ok(aged)) => {
                        HARVEST_SERVED_BLOCKS.fetch_add(aged.len() as u64, Ordering::Relaxed);
                        // Schema 16: the grant's sequence — bumped strictly
                        // AFTER the executor handed the blocks out, so a
                        // lane-free notice queued before any of them could
                        // be free-listed reads a lower `after_grants`. Part
                        // of the cached outcome: a replay answers the same
                        // grant under the same sequence.
                        let grant_seq = next_grant_seq(&client_owned);
                        Ok((aged, grant_seq))
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
        // OQ 2 (schema 8): the reply carries the authority's LIVE bound
        // age — the loop latency in force — so the co-writer's refill
        // horizon is a measurement (0 = nothing held, and the ship side
        // then keeps its derivation). Schema 14: each block's release age
        // beside it (the lane-visible ledger's authority-clock stage).
        PublishCallOutcome::Done(outcome.map(|(aged, grant_seq)| {
            let (blocks, release_ages_ms) = aged.into_iter().unzip();
            PublishReply::LaneFreeGrant {
                blocks,
                bound_age_ms: crate::free_grace::bound_age_ms(),
                release_ages_ms,
                grant_seq,
            }
        }))
    }

    /// PR 5b items 3+4 — the custody verdict for a kvmap-headed serve's
    /// CLAIM SCOPE: `Unscoped` (no owner armed / whole-file custody / a
    /// custody-less writer on a grant-free ino — the claims law still
    /// applies, span-unfiltered), `Scoped` (a RANGE holder: claims filter
    /// to its spans minus demoted regions), or the finding-34 class (a
    /// custody-less writer against OTHER holders' live grants — the
    /// caller refuses on its own counter). Mirrors
    /// [`Self::custody_scoped_layout`]'s shape ladder; the geometry
    /// refusal is the scoped-or-not-at-all law verbatim.
    async fn kvmap_claim_scope(&self, client: &str, ino: u64) -> Result<KvmapClaimScope> {
        let Some(owner) = crate::data_grant::custody_owner() else {
            return Ok(KvmapClaimScope::Unscoped);
        };
        let spans = match owner.client_custody_on(client, ino) {
            crate::data_grant::ClientCustodyShape::Ranges(spans) => spans,
            crate::data_grant::ClientCustodyShape::WholeFile => {
                return Ok(KvmapClaimScope::Unscoped)
            }
            crate::data_grant::ClientCustodyShape::None if owner.ino_has_range_grants(ino) => {
                return Ok(KvmapClaimScope::UnscopedAgainstGrants);
            }
            crate::data_grant::ClientCustodyShape::None => return Ok(KvmapClaimScope::Unscoped),
        };
        let block = match owner.geometry_of(ino).await {
            Some((_size, block)) if block > 0 => block,
            other => {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "S11 ∘ kvmap: range holder '{client}'s publish for ino {ino} cannot be \
                     custody-scoped ({}) — refusing rather than adopting its claims \
                     unscoped (the zeros-interleave law). Arm the authority's range plane \
                     with its geometry source (multi_writer::router_range_geometry)",
                    match other {
                        None => "no geometry source installed",
                        Some(_) => "the geometry source answered block size 0",
                    }
                )));
            }
        };
        Ok(KvmapClaimScope::Scoped {
            spans,
            demoted: crate::dlm::demoted_regions(ino),
            block,
        })
    }

    /// PR 5b item 3 — **the S11 ∘ kvmap scoped compose** (replaces PR 5a's
    /// §11 row-2 refusal): a scoped `SetLayoutAndSize` meeting a kvmap
    /// DURABLE head composes over the TREE-RESOLVED map under the claims
    /// law and persists via the claims-scoped train (item 1's mode)
    /// instead of an inline/blob Put — the head stays sticky-kvmap, every
    /// unclaimed binding survives, and the f36b recompute (item 2) rides
    /// the train. `Ok(None)` = not a kvmap shape (the caller's
    /// inline/blob compose proceeds verbatim).
    ///
    /// Remaining refusals (stated per item 3's contract): a SHIPPED
    /// kvmap head — over a non-kvmap durable base it is a stale/foreign
    /// frame (sticky heads never regress), and as a Put shape at all it
    /// is not a publish the product mints (sticky-head saves ship the
    /// MigrateBlockMap train); and an undecodable legacy/JSON ship over a
    /// kvmap base (the retired "legacy verbatim" arm is exactly the
    /// head-regressing clobber §11 row 2 named).
    async fn try_scoped_kvmap_put(
        &self,
        client: &str,
        ino: u64,
        layout: &[u8],
        size: u64,
        refs: &[BlockRefOp],
    ) -> Result<Option<PublishReply>> {
        use crate::meta_backend::Metadata as _;
        let kvmap_id = |bytes: &[u8]| {
            crate::layout_wire::decode_layout_any(bytes)
                .ok()
                .and_then(|l| l.block_map_id)
                .filter(|id| id.starts_with(crate::meta_backend::kv::block_map::KVMAP_HEAD_PREFIX))
        };
        let durable_kv = match self.inner.getxattr(ino, "layout").await {
            Ok(Some(bytes)) => kvmap_id(&bytes),
            _ => None,
        };
        let shipped_kv = kvmap_id(layout).is_some();
        let Some(durable_id) = durable_kv else {
            if shipped_kv {
                MAP_REFUSED.fetch_add(1, Ordering::Relaxed);
                return Err(SqueezefsError::InvalidOperation(format!(
                    "layout delta base unusable: range holder '{client}'s Put for ino \
                     {ino} ships a kvmap head over a NON-kvmap durable base — sticky \
                     heads never regress, so the frame is stale/foreign; refetch and \
                     recompose (map_refused)"
                )));
            }
            return Ok(None);
        };
        if shipped_kv {
            MAP_REFUSED.fetch_add(1, Ordering::Relaxed);
            return Err(SqueezefsError::InvalidOperation(format!(
                "S11 ∘ kvmap: a kvmap-headed SetLayoutAndSize for ino {ino} is not a \
                 publish shape — a sticky-head save ships the MigrateBlockMap train; \
                 nothing staged (map_refused)"
            )));
        }
        // Decode the shipped Put's map: inline, or rehydrated from its
        // blob (the rung-20 hook). An undecodable non-indirect base
        // refuses — the retired "legacy verbatim" arm is the §11 row-2
        // head-regressing clobber.
        let dec = crate::layout_wire::decode_base_layout(layout);
        let shipped_indirect = matches!(&dec, Err(e) if format!("{e}").contains("indirect"));
        let mut new = match dec {
            Ok(l) => l,
            Err(_) if shipped_indirect => {
                let Some(io) = crate::meta_backend::kv::indirect_map::indirect_map_io() else {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "layout delta base unusable: indirect base — range holder \
                         '{client}'s full Put for ino {ino} ships an indirect map onto a \
                         kvmap head with no indirect-map hook armed (rung 19: refetch \
                         and recompose)"
                    )));
                };
                let mut head = crate::layout_wire::decode_layout_any(layout).map_err(|e| {
                    SqueezefsError::InvalidOperation(format!(
                        "S11 ∘ kvmap: undecodable indirect shipped layout for ino {ino}: {e}"
                    ))
                })?;
                let blob = head
                    .block_map_id
                    .as_deref()
                    .and_then(|id| id.strip_prefix("indirect:"))
                    .map(str::to_string)
                    .ok_or_else(|| {
                        SqueezefsError::InvalidOperation(format!(
                            "S11 ∘ kvmap: indirect shipped layout for ino {ino} names no blob"
                        ))
                    })?;
                let full = (io.read)(blob).await?;
                head.block_map = Some(full.into_iter().collect());
                head
            }
            Err(e) => {
                MAP_REFUSED.fetch_add(1, Ordering::Relaxed);
                return Err(SqueezefsError::InvalidOperation(format!(
                    "S11 ∘ kvmap: range holder '{client}'s Put for ino {ino} is \
                     undecodable as a map source ({e}) — applying it verbatim would \
                     regress the kvmap head to a stale inline value and orphan every \
                     tree-7 record (§11 row 2's clobber); refused, nothing staged \
                     (map_refused)"
                )));
            }
        };
        let mut entries: Vec<(u32, String)> = new
            .block_map
            .take()
            .unwrap_or_default()
            .into_iter()
            .collect();
        entries.sort_unstable_by_key(|&(b, _)| b);
        // The claims (finding 35's law: view ≠ claim), custody-scoped.
        let mut take: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        let mut release: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        for op in refs {
            if op.reference.is_map_blob() || op.reference.owner_ino != ino {
                continue;
            }
            if op.take {
                take.insert(op.reference.block_index);
            } else {
                release.insert(op.reference.block_index);
            }
        }
        match self.kvmap_claim_scope(client, ino).await? {
            KvmapClaimScope::Unscoped => {}
            KvmapClaimScope::Scoped {
                spans,
                demoted,
                block,
            } => {
                let in_custody = |b: &u32| {
                    let bs = u64::from(*b) * block;
                    let be = bs + block;
                    spans.iter().any(|&(s, e)| e > bs && s < be)
                        && !demoted.iter().any(|&(s, e)| e > bs && s < be)
                };
                take.retain(in_custody);
                release.retain(in_custody);
            }
            KvmapClaimScope::UnscopedAgainstGrants => {
                // Finding 34's class, verbatim (the kvmap face): a
                // custody-less Put against other holders' live grants.
                UNSCOPED_PUT_REFUSALS.fetch_add(1, Ordering::Relaxed);
                return Err(SqueezefsError::InvalidOperation(format!(
                    "S11 (finding 34): range-custody-less full Put for ino {ino} from \
                     '{client}' refused — the ino has live range grants held by other \
                     writers (unscoped_put_refusals)"
                )));
            }
        }
        // Finding 28: a claimed entry naming a DEAD incarnation never
        // adopts — the durable binding stands.
        let mut adopt: Vec<(u32, String)> = entries
            .iter()
            .filter(|(b, _)| take.contains(b))
            .cloned()
            .collect();
        retain_live_bindings(&mut adopt, ino);
        let take: std::collections::BTreeSet<u32> = adopt.iter().map(|(b, _)| *b).collect();
        // The flip head: the Put's non-map fields under the STICKY
        // durable kvmap id (the train re-stamps the generation) — never
        // an inline/blob head (§11 row 2's regression).
        new.block_map_id = Some(durable_id);
        new.block_map = None;
        let head_bytes = bincode::serialize(&new).map_err(|e| {
            SqueezefsError::InvalidOperation(format!(
                "S11 ∘ kvmap: flip-head encode failed for ino {ino}: {e}"
            ))
        })?;
        let claims = crate::meta_backend::kv::backend::MapTrainClaims {
            // No generation travels on a Put — the compose reads the
            // CURRENT tree under the serve stripe, so there is no
            // staleness window for the belt to close.
            base_gen: None,
            take,
            release,
            // A served scoped Put: the mw plane's compose (§14 S2
            // pre-fix b) — mints the belt like every shipped train.
            served: true,
            overlay: false,
            window: false,
        };
        match self
            .inner
            .migrate_block_map_train(
                ino,
                &head_bytes,
                size,
                refs,
                &entries,
                crate::routing::map_migrate_chunk(),
                Some(&claims),
                // Claims trains never barrier: a live sweep cursor
                // refuses retried-class inside the train (PR 6b).
                0,
                &|_key, _idx| None,
            )
            .await?
        {
            Some(o) => {
                MAP_SERVED.fetch_add(1, Ordering::Relaxed);
                let released = o.released.len() as u64;
                let mut freed = Vec::new();
                if !o.released.is_empty() {
                    MAP_RECOMPUTED_RELEASES.fetch_add(released, Ordering::Relaxed);
                    note_served_displacements(ino, &displaced_indices_of(ino, &o.released));
                    freed = free_recomputed_releases(ino, o.released, Some(client)).await;
                }
                Ok(Some(PublishReply::PutDone {
                    recomputed: o.recomputed,
                    freed,
                }))
            }
            None => {
                MAP_REFUSED.fetch_add(1, Ordering::Relaxed);
                Err(SqueezefsError::InvalidOperation(format!(
                    "S11 ∘ kvmap: the scoped compose for ino {ino} could not engage the \
                     block-map tree (the bit-16 ratchet failed); nothing was committed"
                )))
            }
        }
    }

    /// **The served migration train** (kvmap PR 2 + PR 5b, design §11
    /// laws a+b): a first crossing/conversion — durable base NOT kvmap —
    /// runs the whole-map train verbatim (there are no tree records to
    /// delete-by-absence, and the shipped map is the crossing's
    /// authority). A sticky-head re-train — durable base kvmap — runs
    /// CLAIMS-SCOPED: the shipper's take/release claims (its whole refs
    /// frame, which a shipped save carries un-chunked — finding 36b) are
    /// the only transitions adopted, so a stale whole-map ship can never
    /// erase a peer's fresh bindings; the carried base generation is
    /// checked against the durable head under the train's held 4a (§11's
    /// belt — a lagging ship refuses retried-class and the shipper
    /// refetches); and the train's f36b recompute replaces the shipper's
    /// frame with the tree→composed swap diff, whose released blocks run
    /// THIS authority's free ladder strictly after commit Ok (the
    /// custody-scoped-Put pattern verbatim).
    #[allow(clippy::too_many_arguments)]
    async fn serve_map_train(
        &self,
        client: &str,
        ino: u64,
        layout: Vec<u8>,
        size: u64,
        entries: Vec<(u32, String)>,
        refs: Vec<WireBlockRefOp>,
        base_gen: u64,
    ) -> Result<PublishReply> {
        use crate::meta_backend::Metadata as _;
        let mut refs: Vec<BlockRefOp> = refs.into_iter().map(BlockRefOp::from).collect();
        // The durable base decides the mode (the KVMAP_HEAD_PREFIX law —
        // never a decode-error probe), under the serve's per-ino stripe.
        let kvmap_base = match self.inner.getxattr(ino, "layout").await {
            Ok(Some(bytes)) => crate::layout_wire::decode_layout_any(&bytes)
                .ok()
                .and_then(|l| l.block_map_id)
                .is_some_and(|id| {
                    id.starts_with(crate::meta_backend::kv::block_map::KVMAP_HEAD_PREFIX)
                }),
            _ => false,
        };
        if !kvmap_base {
            // The finding-34 posture, kept for CROSSING trains (§11 item
            // 4's stated residue): flipping the head mid-episode would
            // take new-crossing whole-map authority nobody arbitrated —
            // the crossing stands down at the caller until custody
            // drains, and a direct train here refuses loud.
            if let Some(owner) = crate::data_grant::custody_owner() {
                if owner.ino_has_range_grants(ino) {
                    MAP_REFUSED.fetch_add(1, Ordering::Relaxed);
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "kvmap crossing for ino {ino} refused — the ino has live range \
                         grants, and a whole-map crossing train would revert peers' \
                         entries (finding 34's class); drain range custody first"
                    )));
                }
            }
        }
        if kvmap_base {
            // §11 law b: the claim sets — the shipper's own transitions
            // (view ≠ claim: everything else in its whole-map ship is a
            // snapshot of blocks it never wrote), custody-scoped (item 4:
            // a RANGE holder's sticky-head ship IS the range-custody path
            // now — the f34 blanket refusal lifted for kvmap bases).
            let mut take: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
            let mut release: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
            for op in &refs {
                if op.reference.is_map_blob() || op.reference.owner_ino != ino {
                    continue;
                }
                if op.take {
                    take.insert(op.reference.block_index);
                } else {
                    release.insert(op.reference.block_index);
                }
            }
            match self.kvmap_claim_scope(client, ino).await? {
                KvmapClaimScope::Unscoped => {}
                KvmapClaimScope::Scoped {
                    spans,
                    demoted,
                    block,
                } => {
                    let in_custody = |b: &u32| {
                        let bs = u64::from(*b) * block;
                        let be = bs + block;
                        spans.iter().any(|&(s, e)| e > bs && s < be)
                            && !demoted.iter().any(|&(s, e)| e > bs && s < be)
                    };
                    take.retain(in_custody);
                    release.retain(in_custody);
                }
                KvmapClaimScope::UnscopedAgainstGrants => {
                    MAP_REFUSED.fetch_add(1, Ordering::Relaxed);
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "kvmap re-train for ino {ino} from '{client}' refused — the ino \
                         has live range grants held by OTHER writers and the shipper \
                         holds no custody (finding 34's class); drain publishes before \
                         releasing range custody (map_refused)"
                    )));
                }
            }
            // Finding 28: a claimed entry naming a DEAD incarnation never
            // adopts — the durable binding stands (the drop shrinks the
            // take set; the entry stays in the shipped map, so the
            // removal law's absence test is untouched).
            let mut adopt: Vec<(u32, String)> = entries
                .iter()
                .filter(|(b, _)| take.contains(b))
                .cloned()
                .collect();
            retain_live_bindings(&mut adopt, ino);
            let take: std::collections::BTreeSet<u32> = adopt.iter().map(|(b, _)| *b).collect();
            let claims = crate::meta_backend::kv::backend::MapTrainClaims {
                base_gen: Some(base_gen),
                take,
                release,
                // A SHIPPED verb executed for a peer: the mw plane's
                // train — mints the belt (§14 S2 pre-fix b).
                served: true,
                overlay: false,
                window: false,
            };
            match self
                .inner
                .migrate_block_map_train(
                    ino,
                    &layout,
                    size,
                    &refs,
                    &entries,
                    crate::routing::map_migrate_chunk(),
                    Some(&claims),
                    // Claims trains never barrier: a live sweep cursor
                    // refuses retried-class inside the train (PR 6b).
                    0,
                    &|_key, _idx| None,
                )
                .await?
            {
                Some(o) => {
                    MAP_SERVED.fetch_add(1, Ordering::Relaxed);
                    // Finding 36b (the kvmap twin): the recompute's
                    // released blocks run this authority's own ladder,
                    // strictly AFTER commit Ok; the reply's `recomputed`
                    // stands the shipper's frame stream down and `freed`
                    // (schema 15) names what the ladder free-listed.
                    let released = o.released.len() as u64;
                    let mut freed = Vec::new();
                    if !o.released.is_empty() {
                        MAP_RECOMPUTED_RELEASES.fetch_add(released, Ordering::Relaxed);
                        note_served_displacements(ino, &displaced_indices_of(ino, &o.released));
                        freed = free_recomputed_releases(ino, o.released, Some(client)).await;
                    }
                    Ok(PublishReply::MapMigrated {
                        records: o.records,
                        record_bytes: o.record_bytes,
                        preexisting: o.preexisting,
                        recomputed: o.recomputed,
                        freed,
                        gen: o.gen,
                    })
                }
                None => {
                    MAP_REFUSED.fetch_add(1, Ordering::Relaxed);
                    Err(SqueezefsError::InvalidOperation(format!(
                        "kvmap re-train for ino {ino} refused — the owner's block-map \
                         tree could not engage (the bit-16 ratchet failed); nothing \
                         was committed"
                    )))
                }
            }
        } else {
            // Finding 36b's owner half: the shipped crossing carries its
            // WHOLE claim set, so the journal-entry-cap chunking (f38's
            // law) runs HERE, under the serve's per-ino stripe.
            const SERVE_REF_TX_CHUNK: usize = 512;
            while refs.len() > SERVE_REF_TX_CHUNK {
                let tail = refs.split_off(SERVE_REF_TX_CHUNK);
                let chunk = std::mem::replace(&mut refs, tail);
                self.inner.commit_block_refs(ino, &chunk).await?;
            }
            match self
                .inner
                .migrate_block_map_train(
                    ino,
                    &layout,
                    size,
                    &refs,
                    &entries,
                    crate::routing::map_migrate_chunk(),
                    None,
                    // A served ESTABLISHING train's durable base is never
                    // kvmap (its sticky-head siblings ride the claims
                    // arms), so no cursor can exist to barrier over; a
                    // violated invariant refuses loud inside the train.
                    0,
                    &|_key, _idx| None,
                )
                .await?
            {
                Some(o) => {
                    MAP_SERVED.fetch_add(1, Ordering::Relaxed);
                    Ok(PublishReply::MapMigrated {
                        records: o.records,
                        record_bytes: o.record_bytes,
                        preexisting: o.preexisting,
                        recomputed: o.recomputed,
                        freed: Vec::new(),
                        gen: o.gen,
                    })
                }
                // The owner's ratchet could not engage the tree —
                // refused loud (nothing committed); the shipper's
                // never-lossy ladder owns the retry.
                None => {
                    MAP_REFUSED.fetch_add(1, Ordering::Relaxed);
                    Err(SqueezefsError::InvalidOperation(format!(
                        "kvmap crossing for ino {ino} refused — the owner's block-map \
                         tree could not engage (the bit-16 ratchet failed); nothing \
                         was committed"
                    )))
                }
            }
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
    /// Finding 35 (`claims`): the caller's OWN refs frame — the exact
    /// transitions its writes performed (the deferred-delta drain + this
    /// save's ops). The compose adopts an in-custody caller entry ONLY
    /// where the frame CLAIMS a take at that index, and removes an
    /// in-custody absent binding only where it claims a release: a full
    /// Put names the caller's WHOLE map, and the un-claimed remainder is
    /// its VIEW of blocks it never wrote — legitimately stale on an aged
    /// file, and adopting it regressed live bindings to prior
    /// still-live keys (the f28 probe only drops DEAD incarnations),
    /// stranding the durable ledger (the two-pass aged-file C8/C2 mint:
    /// take → release → release → re-take across three holders' serves).
    async fn custody_scoped_layout(
        &self,
        client: &str,
        ino: u64,
        shipped: Vec<u8>,
        claims: &[BlockRefOp],
    ) -> Result<ScopedLayout> {
        let verbatim = |shipped: Vec<u8>| ScopedLayout {
            layout: shipped,
            recomputed: None,
            ram_only_releases: Vec::new(),
            blob_custody: ScopedBlobCustody::default(),
        };
        let Some(owner) = crate::data_grant::custody_owner() else {
            return Ok(verbatim(shipped));
        };
        // Finding 35 (second half): the sticky range-episode predicate.
        // A WHOLE-FILE holder's Put was "fully authoritative" (verbatim)
        // — safe when whole-file custody is the ino's ONLY custody story,
        // a stale-view clobber once the ino has run a RANGE episode: the
        // end-of-row closer's flush acquires whole-file custody AFTER
        // every range released, and its map legitimately lags its peers'
        // final publishes (the take-only blob strands + reverted entries
        // the two-pass repro's tape convicted at `recomputed=false`
        // serves). On an episode ino BOTH the whole-file arm and the
        // grant-less arm compose CLAIMS-SCOPED over the whole span —
        // claimed transitions adopt onto the CURRENT durable head, the
        // stale view keeps nothing.
        let episode = crate::dlm::ino_has_range_custody(ino);
        let spans = match owner.client_custody_on(client, ino) {
            crate::data_grant::ClientCustodyShape::Ranges(spans) => spans,
            crate::data_grant::ClientCustodyShape::WholeFile if episode => vec![(0, u64::MAX)],
            crate::data_grant::ClientCustodyShape::None
                if episode && !owner.ino_has_range_grants(ino) =>
            {
                vec![(0, u64::MAX)]
            }
            crate::data_grant::ClientCustodyShape::None if owner.ino_has_range_grants(ino) => {
                // Finding 34 (the s11-blockcyclic C8/C2 storm's root): a
                // full Put from a client with NO grants on an ino OTHER
                // holders hold ranges on. Applied verbatim (the pre-f34
                // shape) it replaced the whole composed map with the
                // shipper's stale view AND staged the shipper's stale
                // refs frame — every peer entry it reverted stranded its
                // durable take ("1 durable vs 0 layout" — the C8 face)
                // and the shipper's duplicate displaced-frees came back
                // refused ("double-release lineage"). Reachable when the
                // shipper's release verb outruns its own backgrounded
                // close-time flush (rung 1 closes that ordering at the
                // release-verb departure gate; this refusal is the
                // owner's shield for every ordering it cannot see).
                // Leak-safe: nothing staged, nothing composed — the
                // shipper's never-lossy ladder keeps custody of the
                // bytes and its retry (or its drained release) resolves.
                UNSCOPED_PUT_REFUSALS.fetch_add(1, Ordering::Relaxed);
                return Err(SqueezefsError::InvalidOperation(format!(
                    "S11 (finding 34): range-custody-less full Put for ino {ino} from \
                     '{client}' refused — the ino has live range grants held by other \
                     writers, and a custody-less Put applied verbatim reverts their \
                     entries to this shipper's stale base while stranding the durable \
                     ledger (the s11-blockcyclic C8 mint). Drain publishes before \
                     releasing range custody (unscoped_put_refusals)"
                )));
            }
            _ => return Ok(verbatim(shipped)),
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
            _ => return Ok(verbatim(shipped)),
        };
        let (cur_dec, new_dec) = (
            crate::layout_wire::decode_base_layout(&durable),
            crate::layout_wire::decode_base_layout(&shipped),
        );
        // PR 5b item 3: a `kvmap:` head on either side never reaches this
        // compose — the SetLayoutAndSize arm routes kvmap-durable bases
        // through `try_scoped_kvmap_put` (the S11 ∘ kvmap compose) and
        // refuses the kvmap-shipped shapes BEFORE calling here.
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
                Err(_) => return Ok(verbatim(shipped)),
            }
        };
        let mut new = if shipped_indirect {
            rehydrate(shipped.clone(), "shipped").await?.0
        } else {
            match new_dec {
                Ok(n) => n,
                Err(_) if durable_indirect => return Err(mixed_refusal("shipped")),
                // JSON-era/undecodable base: the legacy verbatim arm.
                Err(_) => return Ok(verbatim(shipped)),
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
        // Finding 35: the caller's CLAIM sets — the indices its refs
        // frame says it took / released. View ≠ claim: everything else
        // in its full-Put map is a snapshot of blocks it never wrote.
        let mut claim_take: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let mut claim_release: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for op in claims {
            if op.reference.is_map_blob() || op.reference.owner_ino != ino {
                continue;
            }
            if op.take {
                claim_take.insert(op.reference.block_index);
            } else {
                claim_release.insert(op.reference.block_index);
            }
        }
        // Rung 19: the pre-compose head — what the accounting diffs
        // against (the map the composition displaces FROM).
        let head = map.clone();
        // Finding 35 (the removal half): an in-custody binding ABSENT
        // from the caller's map is removed only under a RELEASE claim
        // with no take (a truncate/punch the caller performed) — absence
        // alone is its stale view (an aged-file holder legitimately
        // lacks bindings its peers minted after its last refetch).
        map.retain(|b, _| {
            !(in_custody(*b)
                && !new_map.contains_key(b)
                && claim_release.contains(b)
                && !claim_take.contains(b))
        });
        // Finding 28: the caller's adopted entries pass the binding probe
        // (a dead-incarnation entry drops; the durable's stands).
        // Finding 35 (the adoption half): adoption additionally requires
        // the caller's TAKE claim at the index — an in-custody entry
        // without one is its stale VIEW of a block it never wrote, and
        // adopting it regressed live bindings to prior still-LIVE keys
        // (the f28 probe cannot catch a not-yet-freed displaced key),
        // minting the aged-file strand storm.
        let mut adopt: Vec<(u32, String)> = new_map
            .into_iter()
            .filter(|(b, _)| in_custody(*b) && claim_take.contains(b))
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
            // Finding 15: the caller frame's RAM-only lifetimes — blocks
            // it took AND released inside this frame that neither the
            // head nor the composed map names (see the kv backend's
            // `recompute_refs_against_map`, the same law on the merge
            // arms). The head's keys resolve only when a candidate exists.
            let mut ram_only =
                crate::meta_backend::kv::block_refs::frame_ram_only_candidates(claims, &out);
            if !ram_only.is_empty() {
                let mut named: std::collections::HashSet<(u64, u64)> =
                    std::collections::HashSet::new();
                for (b, k) in head.iter().chain(map.iter()) {
                    if let Some(r) = resolver(k, ino, *b) {
                        named.insert((r.vol_tag, r.block_idx));
                    }
                }
                ram_only.retain(|r| !named.contains(&(r.vol_tag, r.block_idx)));
            }
            (out, ram_only)
        });
        let (refs, ram_only_releases) = match refs {
            Some((ops, ram_only)) => (Some(ops), ram_only),
            None => (None, Vec::new()),
        };
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
            return Ok(ScopedLayout {
                layout: encoded,
                recomputed: refs,
                ram_only_releases,
                blob_custody,
            });
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
            return Ok(ScopedLayout {
                layout: encoded,
                recomputed: refs,
                ram_only_releases,
                blob_custody,
            });
        }
        Ok(ScopedLayout {
            layout: encoded,
            recomputed: refs,
            ram_only_releases,
            blob_custody,
        })
    }

    /// The SetLayoutAndSize serve's PREPARE half (D-1c): everything before
    /// the layout commit — the kvmap scoped-put probe (a reply here means
    /// the call is DONE and never groupable), the custody-scoped compose,
    /// the rung-19/20 refs recomposition, and the over-cap ledger chunks
    /// (their own refs-only transactions, committed here under the serve's
    /// per-ino stripe as before). Returns the staged item plus what the
    /// commit's outcome must settle ([`Self::finish_layout_publish`]).
    async fn prepare_layout_publish(
        &self,
        client: &str,
        ino: u64,
        layout: Vec<u8>,
        size: u64,
        refs: Vec<WireBlockRefOp>,
    ) -> Result<LayoutPrepare> {
        let refs: Vec<BlockRefOp> = refs.into_iter().map(BlockRefOp::from).collect();
        // PR 5b item 3: a kvmap-headed DURABLE base composes over the tree
        // via the claims-scoped train (the S11 ∘ kvmap compose) — the
        // inline/blob compose below never sees a kvmap side.
        if let Some(reply) = self
            .try_scoped_kvmap_put(client, ino, &layout, size, &refs)
            .await?
        {
            // Finding 51: the train committed the caller's claimed takes
            // and displaced its claimed releases.
            note_served_displacements(ino, &displaced_indices(ino, &refs));
            witness_taken(&taken_data_refs(&refs));
            return Ok(LayoutPrepare::Done(reply));
        }
        let ScopedLayout {
            layout,
            recomputed,
            mut ram_only_releases,
            blob_custody,
        } = self
            .custody_scoped_layout(client, ino, layout, &refs)
            .await?;
        // Rung 19: on the SCOPED arm the accounting is the composition's
        // own diff; the caller's MAP-BLOB ops (the indirect blob custody
        // transfer — index-disjoint from map entries) travel verbatim
        // beside it — EXCEPT on the rung-20 compose arm, where the owner
        // recomputes blob custody entirely and the caller's blob ops are
        // DROPPED (staged verbatim they double-count / mis-name blobs the
        // composition renamed). Every verbatim arm keeps the caller's
        // frame byte-identical.
        // Finding 36b: the scoped compose's RELEASED data blocks are the
        // displaced set the COMMIT actually performs — collected here,
        // freed strictly after commit Ok in `finish_layout_publish`
        // (map-blob custody stays on `free_after_commit`).
        let was_recomputed = recomputed.is_some();
        let mut released_data: Vec<crate::meta_backend::kv::block_refs::BlockRef> = Vec::new();
        let mut refs: Vec<BlockRefOp> = match recomputed {
            Some(mut r) => {
                released_data = r
                    .iter()
                    .filter(|o| !o.take && !o.reference.is_map_blob())
                    .map(|o| o.reference)
                    .collect();
                // Finding 15: the frame's RAM-only lifetimes ride the
                // same post-commit ladder (staged nowhere).
                released_data.append(&mut ram_only_releases);
                if !blob_custody.drop_caller_blob_ops {
                    r.extend(refs.iter().filter(|o| o.reference.is_map_blob()).copied());
                }
                r
            }
            None => refs,
        };
        // Finding 36b (the chunk hole's OWNER half): a shipped full-save
        // now carries its WHOLE claim set (the routing pre-chunk stands
        // down for shipped full-saves so the scoped compose above saw
        // every claim), so the journal-entry-cap protection (finding 38)
        // runs HERE: over-cap ledger loads commit FIRST in refs-only
        // transactions under the serve's per-ino stripe (no publish
        // interleaves — SERVE_INO_LOCKS), the tail rides the layout
        // transaction, and a crash between chunk and layout leaves only
        // report-only fsck C8 residue (space-safe, data-safe) — f38's law
        // verbatim, moved to the node whose journal admits the commit.
        const SERVE_REF_TX_CHUNK: usize = 512;
        // Finding 51: the whole frame's takes (chunked or not) are the
        // commit's adopted data blocks — witnessed once the LAYOUT lands;
        // its releases (the recompute's, or the caller's on a verbatim
        // arm) are the indices the commit displaces — screened then too.
        let taken_data = taken_data_refs(&refs);
        let mut displaced = displaced_indices(ino, &refs);
        displaced.extend(displaced_indices_of(ino, &released_data));
        displaced.sort_unstable();
        displaced.dedup();
        while refs.len() > SERVE_REF_TX_CHUNK {
            let tail = refs.split_off(SERVE_REF_TX_CHUNK);
            let chunk = std::mem::replace(&mut refs, tail);
            self.inner.commit_block_refs(ino, &chunk).await?;
        }
        Ok(LayoutPrepare::Staged(PreparedLayoutPublish {
            item: LayoutPublish {
                ino,
                layout,
                size,
                block_refs: refs,
            },
            post: LayoutPostCommit {
                blob_custody,
                released_data,
                taken_data,
                displaced,
                was_recomputed,
            },
        }))
    }

    /// The SetLayoutAndSize serve's FINISH half (D-1c): settle a prepared
    /// item's commit outcome. On `Err` nothing is freed — the fresh blob's
    /// guard drops ARMED (freeing it) and the displaced/released sets stay
    /// untouched, exactly the pre-split `?` shape. On `Ok`: the commit
    /// named the fresh blob, so custody transfers (rung 20), the displaced
    /// durable blob is freed only NOW (the DUR-6 CoW law), and the
    /// compose's released data blocks run this authority's own free
    /// ladder strictly after commit Ok (finding 36b, half 1 — the reply's
    /// `recomputed` stands the caller's frame stream down), and the taken
    /// data blocks reach the binding witness (finding 51) — the serve's
    /// two data-plane reactions, retire and re-publish, both post-commit.
    async fn finish_layout_publish(
        client: &str,
        ino: u64,
        committed: Result<()>,
        post: LayoutPostCommit,
    ) -> Result<PublishReply> {
        let LayoutPostCommit {
            mut blob_custody,
            released_data,
            taken_data,
            displaced,
            was_recomputed,
        } = post;
        committed?;
        // The screen runs BEFORE the recompute's frees: a record whose
        // captured old binding this commit displaced is superseded
        // before that binding's offset can be freed and re-minted.
        note_served_displacements(ino, &displaced);
        witness_taken(&taken_data);
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
        let freed = if released_data.is_empty() {
            Vec::new()
        } else {
            free_recomputed_releases(ino, released_data, Some(client)).await
        };
        Ok(PublishReply::PutDone {
            recomputed: was_recomputed,
            freed,
        })
    }

    async fn execute(&self, client: &str, call: PublishCall) -> Result<PublishReply> {
        match call {
            PublishCall::SetLayoutAndSize {
                ino,
                layout,
                size,
                refs,
                ..
            } => match self
                .prepare_layout_publish(client, ino, layout, size, refs)
                .await?
            {
                LayoutPrepare::Done(reply) => Ok(reply),
                LayoutPrepare::Staged(PreparedLayoutPublish { item, post }) => {
                    let committed = self
                        .inner
                        .set_layout_and_size(item.ino, &item.layout, item.size, &item.block_refs)
                        .await;
                    Self::finish_layout_publish(client, ino, committed, post).await
                }
            },
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
                // Finding 51: the caller's claimed takes are the blocks it
                // DMA'd — witnessed once the chained merge commits; its
                // claimed releases (plus the merge's own recompute below)
                // are the displaced indices the screen sees.
                let taken_data = taken_data_refs(&refs);
                let mut displaced = displaced_indices(ino, &refs);
                // Rung 17 (KD-MW-8's composition law): a SHIPPED merge
                // CHAINS ONTO THE DURABLE HEAD — the claim re-stamps
                // under the backend's own 4a I-guard and the link
                // version re-mints from THIS authority's sequencer, so
                // two co-writers' publishes of one ino compose instead
                // of refusing (the refetch wedge) or re-basing with a
                // private full layout (the s11-range C8 clobber). The
                // reply carries the staged version: the co-writer chains
                // without a refetch.
                let (used, version, released) = self
                    .inner
                    .merge_layout_and_size_chained_accounted(
                        ino,
                        &delta,
                        bytes::Bytes::from(full_layout),
                        size,
                        refs,
                    )
                    .await?;
                if let Some(r) = released.as_deref() {
                    displaced.extend(displaced_indices_of(ino, r));
                    displaced.sort_unstable();
                    displaced.dedup();
                }
                note_served_displacements(ino, &displaced);
                witness_taken(&taken_data);
                // Finding 36 (half 1): the recompute-released DATA blocks
                // run this authority's OWN free ladder strictly AFTER
                // commit Ok (the displaced-blob post-commit pattern) —
                // the caller's frame stood down on the reply flag, so
                // these device frees have exactly one owner. On commit
                // Err the `?` above already returned: nothing is freed.
                let recomputed = released.is_some();
                let freed = match released {
                    Some(released) => free_recomputed_releases(ino, released, Some(client)).await,
                    None => Vec::new(),
                };
                Ok(PublishReply::DeltaUsed {
                    used,
                    version,
                    recomputed,
                    freed,
                })
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
                note_served_displacements(ino, &displaced_indices(ino, &refs));
                witness_taken(&taken_data_refs(&refs));
                Ok(PublishReply::Unit)
            }
            PublishCall::MigrateBlockMap {
                ino,
                layout,
                size,
                entries,
                refs,
                base_gen,
                ..
            } => {
                let frame: Vec<BlockRefOp> = refs.iter().copied().map(BlockRefOp::from).collect();
                let taken_data = taken_data_refs(&frame);
                let displaced = displaced_indices(ino, &frame);
                let reply = self
                    .serve_map_train(client, ino, layout, size, entries, refs, base_gen)
                    .await?;
                note_served_displacements(ino, &displaced);
                witness_taken(&taken_data);
                Ok(reply)
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
