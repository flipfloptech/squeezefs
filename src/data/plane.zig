//! THE DATA PLANE of one mount (charter §6 end to end; §13 rung 4): what
//! a mounted data plane IS — per data volume an `nvme_dev.Device` with its
//! identity re-read through the opened device, a `block_alloc` allocator
//! seeded from the durable reference ledger, the router's placement table,
//! the reclaimer with its device targets; the staging ring and the NVMe
//! read tier under the FORMAT-declared cache paths (none declared ⇒ both
//! OFF, said loud at mount: every write ≤ `inline_max` stays inline and a
//! larger one goes striped); the transform state keyed from the mount's
//! key; the write path (`write_admit.WritePath`), the read path
//! (`read_path.ReadPath`), the warm ladder and `fsync` wired through the
//! seams the portable cores declared. Linux only (the devices).
//!
//! The plane is ALSO the filesystem's data face (`fuse/handler/kv_fs.zig`
//! is generic over it): the layout-record mutations stay in `kv_fs` (they
//! are KV transactions — portable, tested over the RAM store); the plane
//! supplies the block-level acts behind them — the striped write admission
//! per piece, the read of any layout kind, the own-block DMA of a promoted
//! image (`DmaProof`-minting, so a type flip cannot precede its bytes),
//! the terminal frees, the staging ring's `stage`/`release`, the fsync
//! legs, the `statfs` census — and three seams the cores bind to the KV:
//! the block-map AUTHORITY (`seams.MapSeam`: `merge` = ONE transaction,
//! `layout.publishLayout` with the `+ref`/`−ref` records of
//! `block_refs` in the SAME tx, under the per-ino layout lock), the read
//! path's `Meta`, and the read seam over the write side
//! (`write_side_view.Composite` adapted to `read_path.WriteSide`).
//!
//! Locks (charter §2): the plane's per-ino LAYOUT lock is level **(3.7)**
//! — BETWEEN the write side's (3.5) meta stripe and the 4a object locks
//! (`layout.LayoutLocks`; 0002 row 209): every block-map publish takes it
//! INSIDE the upload's stripe (`accumulate`/`device_overlay`/`writeback`
//! hold (3.5) around `MapSeam.merge`), the staged PROMOTION takes the
//! stripe first and the layout lock second (`layout.PublishLocks.
//! lockPromotion`; `pack.commitLocked` runs under both), and kv_fs takes it
//! BEFORE its 4a set. It serializes every layout-record read→stage→commit
//! of an ino: the merge, the crossing, the window merge, the sweep chunk,
//! the promotion — and kv_fs's TRUNCATE transactions (every layout kind)
//! and its tree-mapped CORPSE mark, which take it through the data face
//! (`Data.lockLayout`, from before the layout read until the commit
//! landed) so the tree-mapped head has ONE authority (0002 row 210).
//! kv_fs's write-routing transactions (inline / staged / promote-to-
//! striped) and the destroy rider run under the level-1 exclusive op guard
//! alone; the background promotion meets them only through the identity
//! re-check under both locks. A PARKING mutex (the commit parks), 4096
//! stripes at the 4a load factor. Never held across a terminal block free
//! (RES-1): every displaced binding is freed by the caller after its
//! transaction landed and its locks dropped.
//!
//! The block-map tree (charter §5.6; `data/kvmap.zig`): a merge that would
//! push the inline map past the record cap runs the CROSSING under the
//! per-ino layout lock — the ratchet (`BLOCK_MAP_TREE` stamped and
//! barriered before any record), the A1 residue diff, chunked record txs,
//! the FLIP as the last tx (the head Put with the typed arm that releases no
//! moved reference, plus the window's own `±ref`). A tree-mapped head's
//! merges ship ONLY their window (`kvmap.stageWindow`), reads resolve
//! through the window store, a shrink is size-flip-first with the cursor-
//! resumed sweep (synchronous below `chunk × 64` removed blocks, a meta-
//! lane job above; an unlinked corpse is destroyed by the terminal chunk),
//! and open sweeps are adopted at mount.
//!
//! What this wave does NOT do (recorded in `docs/evidence/rung-04-data-
//! plane.md`): a RAM layout cache (the map authority reads the KV record —
//! latch-free through the node cache, one decode per probe); the writeback
//! spill of parked blocks into the ring. The per-lane transform contexts
//! landed with rung 9 (`Xform`: a codec per executor lane, pooled scratches).

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const Atomic = std.atomic.Value;
const linux = std.os.linux;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const derive = @import("../core/derive.zig");
const errors = @import("../core/errors.zig");
const knobs = @import("../core/knobs.zig");
const log = @import("../core/log.zig");
const budget = @import("../mem/budget.zig");
const exec = @import("../exec/exec.zig");
const lock_order = @import("../sync/lock_order.zig");
const taskset = @import("../sync/taskset.zig");
const nvme_dev = @import("../io/nvme_dev.zig");
// @anchor:ops_zcrx_plane — the zcrx read lane sits behind `Volumes.read_range` (charter §6.3).
const zcrx = @import("../ops/zcrx.zig");
const uring_fs = @import("../io/uring_fs.zig");
const meta_set = @import("../meta/meta_set.zig");
const key = @import("../meta/key.zig");
const record = @import("../meta/record.zig");
const tree = @import("../meta/tree.zig");
const tx_mod = @import("../meta/tx.zig");
const dlm = @import("../meta/dlm.zig");
const jc = @import("../meta/journal_core.zig");
const inos = @import("../meta/inos.zig");
const ino_witness = @import("../meta/ino_witness.zig");
const sym_moves = @import("../meta/sym/slots/moves.zig");
const route_table = @import("../meta/routing/route_table.zig");
const op_guard = @import("../fuse/handler/op_guard.zig");
const fsv = @import("../fuse/transport/filesystem_vtable.zig");
const transport_slot = @import("../fuse/transport/slot.zig");
const zc = @import("../fuse/transport/zc.zig");
const layout = @import("layout.zig");
const kvmap = @import("kvmap.zig");
const merge_scratch = @import("merge_scratch.zig");
const staging = @import("staging.zig");
const pack = @import("pack.zig");
const router = @import("router.zig");
const block_alloc = @import("block_alloc.zig");
const block_refs = @import("block_refs.zig");
const reclaim = @import("reclaim.zig");
const enospc = @import("enospc.zig");
const transform = @import("transform.zig");
const rekey = @import("../ops/defrag/rekey.zig");
const set_config = @import("set_config.zig");
const data_superblock = @import("data_superblock.zig");
const verbs = @import("../wire/verbs.zig");
const record_ship = @import("../meta/sym/xv/record_ship.zig");
const seams = @import("write_seams.zig");
const size_epoch = @import("size_epoch.zig");
const lease_epoch = @import("lease_epoch.zig");
const shape_table = @import("shape_table.zig");
const write_admit = @import("write_admit.zig");
const write_side_view = @import("write_side_view.zig");
const writeback = @import("writeback.zig");
const device_overlay = @import("device_overlay.zig");
const sole_owner_patch = @import("sole_owner_patch.zig");
const accumulate = @import("accumulate.zig");
const extent_overlay = @import("extent_overlay.zig");
const cov = @import("write_coverage.zig");
const fsync_mod = @import("fsync.zig");
const read_path = @import("read_path.zig");
const read_view = @import("read_view.zig");
const read_size_class = @import("read_size_class.zig");
const nvme_read_cache = @import("nvme_read_cache.zig");
const sym_join = @import("../meta/sym/join.zig");
const guard_proc = @import("../meta/guard/proc.zig");
const purge = @import("purge.zig");
const warm_ladder = @import("warm_ladder.zig");
const admission = @import("admission.zig");
const prefetch = @import("prefetch.zig");
const hot_tier = @import("hot_tier.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("data/plane is Linux only (the devices); gate the import on builtin.os.tag");
}

/// The metadata store the plane binds to: the SET facade (charter §5.4 —
/// one or more members behind one store face; every record routes by key).
pub const KvBackend = meta_set.MetaSet;
pub const Mapping = layout.Mapping;
pub const Error = errors.Error;
pub const BlockKey = read_size_class.BlockKey;

// ---------------------------------------------------------------------------
// Constants with their reasons.
// ---------------------------------------------------------------------------

/// The staging segment takes a QUARTER of the cache root's free space
/// (`cache/mod.rs`'s write-half law: the read tier and the host's own
/// files share the device) and never more than an EIGHTH of the memory
/// budget: the ring is a shared mapping whose recovery scan touches every
/// grain, so its resident pages are the daemon's RSS — an eighth leaves
/// the fill pool, the hot tier and the arenas their shares and still holds
/// hundreds of whole-block entries at the venue's budget.
pub const STAGING_SHARE_DIVISOR: u64 = 4;
pub const STAGING_BUDGET_DIVISOR: u64 = 8;
/// The ring must hold at least a few whole-block entries or it is not a
/// ring: 16 × the 4 MiB block.
pub const STAGING_FLOOR: u64 = 64 * derive.MiB;
/// The sole staging segment of this wave (per-path shards are owed).
pub const STAGING_SEGMENT_NAME: [:0]const u8 = "seg-0";

/// THIS mount's staging owner (0003 §T.1): volume 0's appender identity —
/// the manager's `(node, home-volume index)` at appender 0, a joiner's
/// `(node, mount-point slot)` at its appender id, a `--single-writer` D0
/// winner's `(node, 0)` — on the set's identity, with this incarnation's
/// pid. The mount slot is the member scope (its root's name).
fn stagingOwnerMarker(be: *KvBackend) staging.Marker {
    const v0 = be.volumes[0];
    var node: u64 = undefined;
    var slot: u32 = undefined;
    var appender: u32 = undefined;
    if (v0.sym) |set| {
        node = set.identity.node_token;
        slot = set.identity.mount_slot;
        appender = manager_appender_id;
    } else if (v0.joined) |j| {
        node = j.identity.node_token;
        slot = j.identity.mount_slot;
        appender = j.appenderId();
    } else {
        node = sym_join.nodeToken(guard_proc.localIdentity());
        slot = 0;
        appender = manager_appender_id;
    }
    return .{ .set_uuid = be.setUuid(), .set_epoch = be.setEpoch(), .node_token = node, .mount_slot = slot, .appender_id = appender, .owner_pid = @intCast(linux.getpid()) };
}

/// The manager's appender id (the format-time fixed ring's).
const manager_appender_id: u32 = @import("../meta/journal.zig").FIXED_RING_APPENDER;
/// The shape-snapshot table's width (one row per op-guard stripe).
pub const SHAPE_STRIPES: usize = shape_table.STRIPES;

// ---------------------------------------------------------------------------
// Stats.
// ---------------------------------------------------------------------------

pub const stats = struct {
    /// `data_volumes`: active data volumes bound.
    pub var data_volumes: counters.Gauge = .{};
    /// `data_staging_on` / `data_nvme_tier_on`: 1 when the cache paths arm them.
    pub var data_staging_on: counters.Gauge = .{};
    pub var data_nvme_tier_on: counters.Gauge = .{};
    /// `layout_map_merges`: ONE transaction per `MapSeam.merge`.
    pub var layout_map_merges: counters.ShardedCounter = .{};
    pub var layout_map_merge_blocks: counters.ShardedCounter = .{};
    pub var layout_map_merge_failures: counters.ShardedCounter = .{};
    /// Inserts a STALE write-back unit (formed before a size-setting
    /// commit) presented at or past the ino's current EOF: dropped, never
    /// re-inserted, the layout size never raised (`MergeRequest.size_epoch`).
    pub var layout_map_merge_clipped_blocks: counters.ShardedCounter = .{};
    /// `layout_republishes`: the ONE mover's republish transactions.
    pub var layout_republishes: counters.ShardedCounter = .{};
    /// `kvmap_resolve_failures`: a tree-mapped binding the read side could
    /// not resolve (a store error, an undecodable record) — answered as a
    /// miss to the caller, never a fabricated hole without this wire.
    pub var kvmap_resolve_failures: counters.Tripwire = .init("kvmap_resolve_failures");
    /// `kvmap_sweep_spawn_refusals`: a background sweep the executor could
    /// not spawn (it resumes at the next mount from its durable cursor).
    pub var kvmap_sweep_spawn_refusals: counters.ShardedCounter = .{};
    /// `kvmap_head_put_unlocked`: a tree-mapped head Put staged by a
    /// context not holding the (3.7) layout lock (debug builds; must stay
    /// 0 — the head's ONE authority, 0002 row 210).
    pub var kvmap_head_put_unlocked: counters.Tripwire = .init("kvmap_head_put_unlocked");
    /// `layout_map_merge_displaced_overflow`: an inline-map fold displaced
    /// more bindings than `MergeOutcome` holds (the excess would never be
    /// freed). A fold displaces at most one binding per insert and a merge
    /// carries at most `max_displaced` inserts — must stay 0.
    pub var layout_map_merge_displaced_overflow: counters.Tripwire = .init("layout_map_merge_displaced_overflow");
    /// The promotion dispatch at dismount / pressure / fsync.
    pub var promotions_run: counters.ShardedCounter = .{};
    pub var promotions_stale: counters.ShardedCounter = .{};
    pub var promotions_failed: counters.ShardedCounter = .{};
    /// Own-block images landed for a layout promotion (staged/inline → striped).
    pub var own_block_dmas: counters.ShardedCounter = .{};
    pub var own_block_dma_bytes: counters.ShardedCounter = .{};
    /// The promotion arms' data barrier (DUR-1, `promotion_barriers`):
    /// `promotion_barriers` — barrier requests between a promotion's DMA
    /// and its publish; `promotion_barriers_coalesced` — the requests a
    /// flush another promotion led answered (flushes issued = requests −
    /// coalesced); `promotion_barrier_write_through_skips` — a write-through
    /// volume owes none. The cover rounds that release the departed ring
    /// copies afterwards: `promotion_cover_rounds_failed` (a meta barrier
    /// that failed; the copies stay held for the next round).
    pub var promotion_barriers: counters.ShardedCounter = .{};
    /// The staged fast path's point cache (`stagedPoint`): hits served
    /// without a lookup, misses that went to the read view and the store.
    pub var staged_point_hits: counters.ShardedCounter = .{};
    pub var staged_point_misses: counters.ShardedCounter = .{};
    pub var promotion_barriers_coalesced: counters.ShardedCounter = .{};
    pub var promotion_barrier_write_through_skips: counters.ShardedCounter = .{};
    pub var promotion_cover_rounds_failed: counters.ShardedCounter = .{};
    /// Direct stores of a held slot (`WRITE_FIXED(device ← slot)`) through
    /// the seam, and their bytes (held-store bytes: every one a byte no
    /// daemon pass touched).
    pub var held_stores: counters.ShardedCounter = .{};
    pub var held_store_bytes: counters.ShardedCounter = .{};
    pub var held_store_failures: counters.ShardedCounter = .{};
    /// Hold gate verdicts on the queue thread; `hold_gate_overlay_admits`
    /// is the overlay class's share of the admits (the rest W1's).
    pub var hold_gate_admits: counters.ShardedCounter = .{};
    pub var hold_gate_overlay_admits: counters.ShardedCounter = .{};
    pub var hold_gate_declines: counters.ShardedCounter = .{};
    /// Prefetch / ceremony fibers spawned on the home lanes.
    pub var prefetch_fibers: counters.ShardedCounter = .{};
    pub var prefetch_fiber_refusals: counters.ShardedCounter = .{};
    pub var hold_ceremony_fibers: counters.ShardedCounter = .{};
    /// Terminal frees handed to the reclaimer by the filesystem.
    pub var bindings_freed: counters.ShardedCounter = .{};
    /// A released binding freed with no in-flight registration (decision
    /// 0003 §AH.1: every release site holds its blocks from before its
    /// commit until after their frees; must stay 0).
    pub var release_free_unheld: counters.Tripwire = .init("release_free_unheld");
    /// The mount-time C8 oracle's verdict (0 = clean).
    pub var block_refs_drift_rows: counters.Gauge = .{};
    /// Device reads the plane bounced for an unaligned window.
    pub var device_read_bounces: counters.ShardedCounter = .{};
    /// The device-read ledger's TOTAL at the plane's one read choke point
    /// (decision 0003 §K): every byte a data device DMA'd for any purpose,
    /// bounce padding included. Closure: `data_device_read_bytes ≡ Σ
    /// read_path.read_dma_* + write_side_read_dma_bytes +
    /// compose_read_dma_bytes + pack_read_dma_bytes`.
    pub var data_device_read_bytes: counters.ShardedCounter = .{};
    /// Device bytes the WRITE side read through its seam (overlay serves
    /// and gap seeds, the fold/flush/spill seed images, the epoch's old
    /// image) — grain-rounded.
    pub var write_side_read_dma_bytes: counters.ShardedCounter = .{};
    /// Device bytes the read seam's COMPOSITION read behind a READ: a
    /// partial's gaps from the base, a redirect's shadow window —
    /// ranged, never the whole seed block.
    pub var compose_read_dma_bytes: counters.ShardedCounter = .{};
    /// Device bytes the pack's own read seam moved (tenant copies).
    pub var pack_read_dma_bytes: counters.ShardedCounter = .{};
    /// A device read on a volume tag no mounted volume carries (must stay 0).
    pub var unresolved_volume: counters.Tripwire = .init("data_unresolved_volume");
    /// A foreign-slot file's block-map merge SHIPPED to its holder as one
    /// layout publish (charter §5.5), and the kvmap shapes refused on a
    /// foreign file (a tree-mapped head / a crossing: the records are the
    /// holder's slot tree's — write a PB-class file from its holder).
    pub var layout_map_merges_shipped: counters.ShardedCounter = .{};
    pub var foreign_publish_kvmap_refusals: counters.ShardedCounter = .{};
    /// Shipped publishes served here: the record and its refs staged
    /// through the plane's face.
    pub var layout_publishes_served: counters.ShardedCounter = .{};
};

pub fn writeStatsJson(w: *Io.Writer) Io.Writer.Error!void {
    try w.writeAll("{");
    inline for (.{ "data_volumes", "data_staging_on", "data_nvme_tier_on", "block_refs_drift_rows" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    inline for (.{ "layout_map_merges", "layout_map_merge_blocks", "layout_map_merge_failures", "layout_map_merge_clipped_blocks", "layout_map_merges_shipped", "foreign_publish_kvmap_refusals", "layout_publishes_served", "kvmap_sweep_spawn_refusals", "promotions_run", "promotions_stale", "promotions_failed", "own_block_dmas", "own_block_dma_bytes", "promotion_barriers", "staged_point_hits", "staged_point_misses", "promotion_barriers_coalesced", "promotion_barrier_write_through_skips", "promotion_cover_rounds_failed", "held_stores", "held_store_bytes", "held_store_failures", "hold_gate_admits", "hold_gate_overlay_admits", "hold_gate_declines", "prefetch_fibers", "prefetch_fiber_refusals", "hold_ceremony_fibers", "bindings_freed", "device_read_bounces", "data_device_read_bytes", "write_side_read_dma_bytes", "compose_read_dma_bytes", "pack_read_dma_bytes" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    try w.print("\"data_unresolved_volume\":{d},\"kvmap_resolve_failures\":{d},\"kvmap_head_put_unlocked\":{d},\"release_free_unheld\":{d},\"layout_map_merge_displaced_overflow\":{d}}}", .{ counters.tripwireCount("data_unresolved_volume"), counters.tripwireCount("kvmap_resolve_failures"), counters.tripwireCount("kvmap_head_put_unlocked"), counters.tripwireCount("release_free_unheld"), counters.tripwireCount("layout_map_merge_displaced_overflow") });
}

// ---------------------------------------------------------------------------
// The shape table (`shape_table.zig`): what a thread may know about an inode
// without a KV lookup.
// ---------------------------------------------------------------------------

const ShapeTable = shape_table.ShapeTable;

// ---------------------------------------------------------------------------
// The plane.
// ---------------------------------------------------------------------------

/// The held-slot store face the session installs once it is armed.
pub const HeldStoreFace = struct {
    ctx: *anyopaque,
    store: *const fn (ctx: *anyopaque, src: transport_slot.HeldSource, fd: linux.fd_t, off: u64, io: Io) transport_slot.ExtractError!void,
};

const Staging = struct {
    /// THIS member's root under the cache path, its live claim held for
    /// the ring's life (0003 §T.1).
    root: staging.Ring.Bound,
    /// The marker the bind stamped (the online fsck's C5 re-reads against it).
    marker: staging.Marker,
    mapped: staging.Ring.MappedSegment,
    seg: staging.Segment,
    pool: staging.Pool,
    /// A sweep task is spawned or running (`sweepRing`): one at a time.
    sweep_armed: Atomic(bool) = .init(false),
};

/// The transformed set's state (decision 0003 §K10): the immutable
/// transform, one codec per executor lane (a decode on any lane takes a
/// codec with no lock), and the stored-image scratches the plane's own
/// transforms hold across their DMAs (`transformWrite` → the own-block /
/// pack DMA; `readMappingImage`'s decode destination).
const Xform = struct {
    t: transform.Transform,
    crypto: ?*transform.CryptoState,
    codecs: transform.CodecPool,
    scratches: transform.ScratchPool,
};

pub const Space = struct { total_bytes: u64, free_bytes: u64 };

pub const Options = struct {
    env: *const std.process.Environ.Map,
    facts: derive.Facts,
    budget_bytes: u64,
    /// The volume key (an encrypted set); zeroized by the caller after.
    derived_key: ?transform.DerivedKey,
    /// An open rotation window's NEXT key (`set_config.next_key`): new
    /// writes wrap under it, the primary becomes the unwrap-only arm.
    derived_next: ?transform.DerivedKey = null,
    /// The level-1 guard table the warm ladder try-locks.
    guards: *op_guard.Table,
};

pub const OpenError = error{ OutOfMemory, DeviceRefused, IdentityMismatch, Io, StagingRefused, StagingOwnerLive, StagingForeignOwner, TransformRefused, ReclaimRefused, InlineMaxAboveFormatBound };

pub const DataPlane = struct {
    /// The (3.7) guard kv_fs holds across a truncate / corpse transaction.
    pub const LayoutGuard = layout.LayoutLocks.Guard;

    /// The own-block act's result: the whole-block mapping and the proof
    /// the layout publish consumes.
    pub const OwnBlock = struct {
        mapping: Mapping,
        proof: layout.DmaProof,
        alloc: *block_alloc.BlockAllocator,
        idx: u64,
    };

    gpa: Allocator,
    io: Io,
    sqz: *exec.SqzIo,
    backend: *KvBackend,
    env: *const std.process.Environ.Map,
    block_size: u64,
    chunk: u64,
    inline_max: u64,
    geo: layout.Geometry,
    name_key: *const key.NameKey,
    passthrough: bool,
    vols: seams.Volumes = .{},
    devs: [router.MAX_VOLUMES]?*nvme_dev.Device = @splat(null),
    /// The zcrx read lane (charter §6.3): the funnel the read path's volume
    /// seam runs through, the per-volume sessions, the NIC registry.
    zcrx_funnel: zcrx.funnel.Funnel = .{},
    zcrx_decorated: zcrx.funnel.Decorated = undefined,
    zcrx_bound: [router.MAX_VOLUMES]?*zcrx.arm.Bound = @splat(null),
    zcrx_registry: zcrx.steering.Registry,
    rt: router.Router,
    reclaimer: *reclaim.Reclaimer,
    reclaim_free: seams.ReclaimFree,
    /// Levels (3.5) and (3.7) with the promotion's order (module doc).
    locks: layout.PublishLocks = .{},
    /// The staged fast path's per-ino cache (`stagedPoint`).
    staged_points: StagedPointCache = .{},
    /// The promotion arms' data barrier per volume (`fsync.BarrierCoalescer`,
    /// DUR-1: the DMA'd block flushed BEFORE the layout naming it publishes;
    /// concurrent promotions on a volume ride one flush).
    promotion_barriers: [router.MAX_VOLUMES]fsync_mod.BarrierCoalescer = @splat(.{}),
    /// The block-map publish's pooled working set (`merge_scratch`): the
    /// record, the fold and the tx's arena of one merge, never the heap.
    publish_scratch: merge_scratch.Pool,
    stage: ?Staging = null,
    xform: ?Xform = null,
    packer: ?pack.Packer = null,
    wp: *write_admit.WritePath,
    rp: *read_path.ReadPath,
    ladder: warm_ladder.WarmLadder,
    held_store: ?HeldStoreFace = null,
    /// Prefetch / ceremony fibers on the home lanes; awaited at close.
    lane_group: Io.Group = .init,
    /// Promotion tasks (the pressure hook) on the meta pool.
    tasks: taskset.TaskSet = .init,
    shape: ShapeTable,
    /// The per-ino read view (`read_view.zig`): what an op needs of an
    /// inode's metadata without a KV lookup, staled by the ino witness.
    view: read_view.Table,
    /// The per-ino size epochs (`size_epoch.SizeEpochs`): bumped by every
    /// size-setting commit (`noteSizeSet`), read at a write's landing
    /// (`write_admit.Outcome.size_epoch`) and a write-back unit's formation,
    /// judged at the raise (`sizeSetSince`, the merge's stale rule).
    size_epochs: size_epoch.SizeEpochs,
    file_id_seq: Atomic(u64),
    refs_store: block_refs.RefStore,
    scope: u64,
    /// The block-map tree: record ops per crossing / sweep tx (the one
    /// lever), the read side's window store, the sweep jobs in flight (the
    /// submission dedup) and their group, the stop latch `close` sets.
    kvmap_chunk: u32,
    windows: kvmap.WindowCache,
    sweeps_lock: jc.SpinLock = .{},
    sweeps: std.AutoHashMapUnmanaged(u64, void) = .empty,
    sweep_group: Io.Group = .init,
    sweeps_stop: Atomic(bool) = .init(false),
    meta_rr: Atomic(u64) = .init(0),
    /// `SQUEEZEFS_TEST_KVMAP_CROSSING_KILL_AFTER` (0 = inert).
    crossing_kill_after: u32,
    /// `SQUEEZEFS_TEST_OWN_BLOCK_POISON`.
    test_own_block_poison: bool,
    /// The token divert's layout face (bound by the daemon once the wire
    /// plane is up; null = a flat mount: every slot local).
    foreign_layout: ?ForeignLayout = null,
    /// The foreign-slot layout PUBLISH's ship (charter §5.5 "Record-level
    /// mutations of a foreign-slot file — the layout publish"): bound with
    /// the layout face; null = a merge on a foreign record is refused.
    foreign_publish: ?ForeignPublish = null,
    /// The filesystem's parked size claims (decision 0003 §AL.5): an
    /// extending write's size is readable before it is durable, so the
    /// plane's reads and its write side fold it as the attr replies do.
    claims: ?ClaimSource = null,
    /// `dismount` ran (once; `close` runs it if nobody did).
    dismounted: bool = false,

    /// A foreign-slot layout record through the holder's token.
    pub const ForeignLayout = struct {
        ctx: *anyopaque,
        lookup: *const fn (ctx: *anyopaque, gpa: Allocator, ino: u64) Error!Verdict,

        pub const Verdict = union(enum) {
            /// This mount's slot: read the store.
            local,
            /// Gone at its holder.
            gone,
            /// The token page: the inode's size and the carried `layout`
            /// xattr's value when one is held (owned by `gpa`).
            foreign: struct { size: u64, layout: ?[]u8 },
        };
    };

    /// Bind (or, with null, unbind BEFORE the seam's owner goes) the
    /// foreign-layout face; the dismount pass (`dismount`) reads layouts
    /// through it when bound.
    pub fn bindForeignLayout(self: *DataPlane, f: ?ForeignLayout) void {
        self.foreign_layout = f;
    }

    /// The token recall's data-plane half (`wire_plane.RecallSink`; spec
    /// `sym_tokens_intents_striping.md` §1.7 step 2): the recalled
    /// object's carried `layout` value (the raw layout, as the token
    /// carries it) — every block key it names leaves the read tiers through
    /// the ONE purge. Null when it cannot be enumerated (it does not
    /// decode, or a block-map tree head names its references elsewhere):
    /// the caller runs the census.
    pub fn recallPurgeLayout(self: *DataPlane, value: []const u8) ?u32 {
        const l = layout.Layout.decode(value, self.geo) catch return null;
        return self.rp.purgeRecalledLayout(&l);
    }

    /// The recall's census fallback: every FOREIGN key the tiers hold
    /// (`ReadPath.purgeForeignCensus`).
    pub fn recallPurgeCensus(self: *DataPlane) u32 {
        return self.rp.purgeForeignCensus();
    }

    /// A foreign-slot file's layout publish shipped to its holder as ONE
    /// record verb: the encoded layout record and the writer's DMA proofs
    /// behind every block it newly names.
    pub const ForeignPublish = struct {
        ctx: *anyopaque,
        ship: *const fn (ctx: *anyopaque, ino: u64, value: []const u8, proofs: []const verbs.DmaProofWire) errors.Error!void,
    };

    pub fn bindForeignPublish(self: *DataPlane, f: ?ForeignPublish) void {
        self.foreign_publish = f;
    }

    /// The HOLDER's side of a shipped publish (`record_ship.PublishFace`):
    /// the layout record staged through `publishLayout` with its `±ref`
    /// diff against the record in place — the writer's proofs re-minted
    /// from the wire words, so a block without its DMA witness refuses.
    pub fn servedPublishFace(self: *DataPlane) record_ship.PublishFace {
        return .{ .ctx = self, .stage = &servedPublishStage };
    }

    fn servedPublishStage(ctx: *anyopaque, t: *tx_mod.KvTx, key_ino: u64, coll_seq: u8, prev_bytes: ?[]const u8, value: []const u8, proofs: []align(1) const verbs.DmaProofWire) record_ship.PublishFace.PublishError!void {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const prev: ?layout.Layout = if (prev_bytes) |pb| (layout.XattrLayout.decode(pb, self.geo) catch return error.Rejected) else null;
        const new = layout.Layout.decode(value, self.geo) catch return error.Rejected;
        var pbuf: [verbs.PUBLISH_PROOFS_MAX]layout.DmaProof = undefined;
        for (proofs, 0..) |w, i| pbuf[i] = .{ .vol_tag = w.vol_tag, .offset = w.offset, .len = w.len, .completed_ns = w.completed_ns };
        layout.publishLayout(t, .{ .ino = key_ino, .name_key = self.name_key, .coll_seq = coll_seq, .prev = prev, .layout = new, .proofs = pbuf[0..proofs.len] }, self.refsSeam(), self.geo) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.ProofMissing => error.ProofMissing,
            else => error.Rejected,
        };
        stats.layout_publishes_served.inc();
    }

    // ---- open / close ----

    /// Bind the set `cfg` names: devices, allocators, the ledger seed, the
    /// router, the reclaimer, the staging ring + the NVMe tier (iff cache
    /// paths), the transform, the write and read paths, the warm ladder.
    pub fn open(gpa: Allocator, io: Io, sqz: *exec.SqzIo, be: *KvBackend, cfg: *const set_config.Config, opts: Options) OpenError!*DataPlane {
        const node_size: u64 = be.nodeSize();
        const inline_max = layout.resolveInlineMax(if (cfg.inline_max != 0) cfg.inline_max else knobs.optIntKnob("SQUEEZEFS_INLINE_MAX_BYTES", u64, opts.env), opts.facts, node_size) catch return error.InlineMaxAboveFormatBound;
        const tcfg = cfg.transformCfg();
        const self = try gpa.create(DataPlane);
        errdefer gpa.destroy(self);
        self.* = .{
            .gpa = gpa,
            .io = io,
            .sqz = sqz,
            .backend = be,
            .env = opts.env,
            .block_size = cfg.block_size,
            .chunk = cfg.chunk_size,
            .inline_max = inline_max,
            .geo = .{ .chunk = cfg.chunk_size, .inline_ceiling = layout.inlineMaxCeiling(node_size) },
            .name_key = be.nameKey(),
            .passthrough = tcfg.isPassthrough(),
            .rt = undefined,
            .reclaimer = undefined,
            .reclaim_free = undefined,
            .wp = undefined,
            .rp = undefined,
            .ladder = undefined,
            .shape = undefined,
            .view = undefined,
            .size_epochs = undefined,
            .file_id_seq = .init(@bitCast(clock.realtimeCoarseNs())),
            .refs_store = block_refs.RefStore.from(KvBackend, be),
            // Rung 4: one meta volume, the native slot — scope 0.
            .scope = pack.packScopeKey(0, 0),
            .kvmap_chunk = kvmap.chunkFromEnv(opts.env),
            .windows = undefined,
            .crossing_kill_after = knobs.intKnob("SQUEEZEFS_TEST_KVMAP_CROSSING_KILL_AFTER", u32, opts.env),
            .test_own_block_poison = knobs.boolKnob("SQUEEZEFS_TEST_OWN_BLOCK_POISON", opts.env),
            .zcrx_registry = zcrx.steering.Registry.init(gpa),
            .publish_scratch = undefined,
        };
        self.publish_scratch = try merge_scratch.Pool.init(gpa, io, merge_scratch.poolCap(opts.facts.cpus), merge_scratch.slabLen(@intCast(node_size)));
        errdefer self.publish_scratch.deinit();
        budget.authority.register(self.publish_scratch.component());
        self.shape = try ShapeTable.init(gpa);
        errdefer self.shape.deinit(gpa);
        self.view = try read_view.Table.init(gpa, record.xattrValueCap(@intCast(node_size)), self.geo);
        errdefer self.view.deinit();
        budget.authority.register(self.view.component());
        self.size_epochs = try size_epoch.SizeEpochs.init(gpa, SHAPE_STRIPES);
        errdefer self.size_epochs.deinit();
        self.windows = try kvmap.WindowCache.init(gpa, kvmap.windowBudgetBytes(opts.budget_bytes), cfg.chunk_size);
        errdefer self.windows.deinit();
        budget.authority.register(self.windows.component());
        self.rt = try router.Router.init(gpa, cfg.chunk_size);
        errdefer self.rt.deinit();

        // 1. Devices + allocators + the ledger seed.
        var opened: u32 = 0;
        errdefer self.closeVolumes(opened);
        for (cfg.data) |d| {
            if (d.state == .retired) continue;
            if (opened == router.MAX_VOLUMES) break;
            // The router mirrors the DURABLE lifecycle state (a volume left
            // Draining by a killed drain must read Draining to the adopted
            // evacuation, never Active).
            try self.openVolume(d, opts.env, &opened, switch (d.state) {
                .active => .active,
                .draining => .draining,
                .disabled => .disabled,
                .retired => unreachable,
            });
        }
        stats.data_volumes.set(opened);

        // 2. The reclaimer: every volume a target; the tier purge, the fence
        //    probe and the foreground signal; the worker + the drainer.
        self.reclaimer = reclaim.Reclaimer.init(gpa, reclaim.Config.fromEnv(opts.env, opts.budget_bytes)) catch return error.OutOfMemory;
        errdefer self.reclaimer.deinit();
        for (self.vols.rows[0..self.vols.n()], 0..) |r, i| {
            const v = r.?;
            self.reclaimer.registerTarget(@intCast(i), v.alloc, reclaim.device_binding.target(self.devs[i].?));
        }
        self.reclaimer.setTierPurge(.{ .ctx = self, .purge = &reclaimPurge });
        self.reclaimer.setFenceProbe(.{ .ctx = self, .poll = &fencePoll });
        self.reclaimer.setForegroundSignal(.{ .ctx = null, .read = &foregroundBytes });
        for (self.vols.rows[0..self.vols.n()]) |r| r.?.alloc.space_pending = self.reclaimer.spacePending();
        self.reclaim_free = .{ .r = self.reclaimer, .vols = &self.vols };
        self.reclaimer.start(io) catch return error.ReclaimRefused;
        errdefer self.reclaimer.stopAndJoin(io);

        // 3. The transform (an encrypted/compressed set).
        if (!self.passthrough) try self.openTransform(gpa, io, cfg, tcfg, opts);
        errdefer self.closeTransform();

        // 4. Staging + the NVMe tier under THIS member's root of the cache
        //    path (or OFF, loud). A `-o ro` reader stages nothing and binds
        //    no member's root (0003 §T.1).
        var nvme: nvme_read_cache.Cache = nvme_read_cache.Cache.initDisabled(gpa);
        if (cfg.cache_paths.len > 0 and opened > 0 and be.isReader()) {
            log.info("data plane: a `-o ro` reader binds no member's staging root — staging and the NVMe read tier are OFF on this mount", .{});
        } else if (cfg.cache_paths.len > 0 and opened > 0) {
            try self.openStaging(gpa, io, cfg, opts);
            nvme = self.openNvmeTier(gpa, opts);
        } else {
            log.info("data plane: the set declares NO cache paths — staging and the NVMe read tier are OFF: a write ≤ inline_max ({d} B) stays inline, anything larger goes STRIPED (no packed layout, no promotion pass at dismount)", .{inline_max});
        }
        errdefer self.closeStaging();
        stats.data_staging_on.set(@intFromBool(self.stage != null));
        stats.data_nvme_tier_on.set(@intFromBool(nvme.enabled));

        // 5. The read path over the plane's seams.
        var rcfg = read_path.Config.fromEnv(opts.env, cfg.block_size, self.passthrough, opts.facts, opts.budget_bytes);
        if (opened == 0) rcfg.read_lane = false;
        const decode: ?read_path.Decode = if (self.xform != null) .{ .ctx = self, .chunk = cfg.chunk_size, .decode = &decodeImage, .frame_span = &decodeFrameSpan, .decode_range = &decodeRange } else null;
        self.rp = read_path.ReadPath.init(gpa, rcfg, self.meta(), self.readVolumes(), self.writeSide(), decode, nvme, &budget.authority) catch |e| switch (e) {
            error.OutOfMemory => return error.OutOfMemory,
            else => return error.Io,
        };
        errdefer self.rp.deinit();
        self.rp.registerComponents(&budget.authority);
        self.rp.issuer = .{ .ctx = self, .issue = &issuePrefetch };

        // 6. The write path.
        const wcfg = write_admit.Config.fromEnv(opts.env, cfg.block_size, opts.budget_bytes);
        self.wp = write_admit.WritePath.init(gpa, wcfg, .{
            .io = io,
            .vols = &self.vols,
            .device = self.deviceSeam(),
            .map = self.mapSeam(),
            .free = self.reclaim_free.seam(),
            .purge = self.purgeSeam(),
            .valve = enospc.Valve.fromReclaimer(self.reclaimer),
            .stripes = &self.locks.stripes,
            .mem = &budget.authority,
            .budget_bytes = opts.budget_bytes,
            .transform = self.transformSeam(),
            .scratch_len = if (self.xform != null) transform.scratchLen(cfg.block_size) else 0,
            .scratch_pool_cap = transform.scratchPoolCap(opts.facts.cpus),
            .image = if (self.xform != null) seams.ImageSeam{ .ctx = self, .read = &imageSeamRead } else null,
            .fence = .{ .ctx = self, .poll = &fencePoll },
        }) catch return error.OutOfMemory;
        errdefer self.wp.deinit(io);
        // The write side's R5 components (spec §2.5): the parked-buffer
        // pool's RAM (its cached cells return under the idle-trim law; the
        // drain-target shed is the write path's to bind) and the in-flight
        // DMA bytes (non-sheddable, drain by completion).
        budget.authority.register(self.wp.table.component(null));
        // The W2 page overlays' RAM (charter §6.2 item 4: "charged to the
        // budget as `parked_extent_bytes`"): attribution; the fold at the
        // RAM trigger and the parked cap's Red halving are its response.
        budget.authority.register(budget.Component.init("parked_extents", 0, 0, .{ .gauge = &extent_overlay.stats.parked_extent_bytes }, null));
        budget.authority.register(self.wp.governor.component());

        // 7. The packer (the promotion primitive's packed arm).
        if (self.stage != null) {
            self.packer = pack.Packer.init(gpa, cfg.chunk_size, pack.Policy.fromEnv(opts.env, cfg.chunk_size), self.packAllocSeam(), self.dmaSeam(), self.packReadSeam(), router.MAX_VOLUMES) catch return error.OutOfMemory;
        }
        errdefer if (self.packer) |*p| p.deinit();

        // 8. The warm ladder (the probe arm); the zc legs arm with the session.
        self.ladder = .{ .rp = self.rp, .guards = opts.guards, .io = io, .ceremony = .{ .ctx = self, .spawn = &spawnCeremony } };
        for (self.vols.rows[0..self.vols.n()]) |r| {
            const v = r.?;
            self.rt.updateCensus(v.vol_tag, v.alloc.usedBlocks() * cfg.block_size) catch {};
        }
        // The C8 oracle at mount (the A/B lever): the derived walk vs the
        // ledger; the C11 record walk (report-only) rides the same lever.
        if (block_refs.verifyAtMount(opts.env)) {
            self.verifyRefsAtMount();
            self.verifyBlockMapsAtMount();
        }
        // Open sweeps (a shrink or an unlink the previous daemon left mid-
        // sweep) regenerate their jobs from the durable cursor.
        self.adoptSweeps();
        log.info("data plane: {d} data volume(s), block {d} B, chunk {d} B, inline_max {d} B, inline map cap {d} entries, map chunk {d} ops, sweep threshold {d} blocks, transform {s}/{s}, staging {s}, NVMe tier {s}, direct-store face pending the session", .{ opened, cfg.block_size, cfg.chunk_size, inline_max, self.inlineMapCap(), self.kvmap_chunk, kvmap.sweepThresholdBlocks(self.kvmap_chunk), @tagName(cfg.compression), cfg.encryption.name(), if (self.stage != null) "ON" else "off", if (nvme.enabled) "ON" else "off" });
        return self;
    }

    /// The ONLINE `volume add-data` arm (charter §8 "online data add"): the
    /// device the lattice just formatted and recorded joins the live plane
    /// — opened, its allocator seeded (empty), the reclaimer's target, the
    /// router row DISABLED (no placement until the caller armed the grant
    /// plane over it and the lattice published `Active`) — visible to every
    /// lock-free reader through `Volumes.add`'s release store. Returns the
    /// ordinal the grant plane arms.
    pub fn attachVolume(self: *DataPlane, d: set_config.DataVolume) OpenError!u16 {
        var opened = self.vols.n();
        if (opened >= router.MAX_VOLUMES) return error.DeviceRefused;
        const ordinal: u16 = @intCast(opened);
        try self.openVolume(d, self.env, &opened, .disabled);
        const v = self.vols.rows[ordinal].?;
        self.reclaimer.registerTarget(ordinal, v.alloc, reclaim.device_binding.target(self.devs[ordinal].?));
        v.alloc.space_pending = self.reclaimer.spacePending();
        stats.data_volumes.set(opened);
        return ordinal;
    }

    /// The router's fill census off every allocator (the fill term of the
    /// placement weights, `backend_fill_spread`, the planners' rows): the
    /// cadence tick's and every planner's call — the census at open alone
    /// went stale with the first write of the mount.
    pub fn refreshCensus(self: *DataPlane) Allocator.Error!void {
        for (self.vols.rows[0..self.vols.n()]) |r| {
            const v = r.?;
            self.rt.updateCensus(v.vol_tag, v.alloc.usedBlocks() * self.block_size) catch {};
        }
        try self.rt.refreshPlacement();
    }

    /// Index 0 of every data volume is the data superblock chunk, allocated
    /// forever at open (`recoverBlock(0)`); every "file blocks on the
    /// volume" census subtracts it.
    pub const RESERVED_BLOCKS: u64 = 1;

    /// The lifecycle's ONE runtime flip (the lattice's `publishPlacement`):
    /// the router's state (placement) and the volume row's custody flag
    /// (the write path takes no new custody on a non-Active volume), then
    /// the placement table re-derived.
    pub fn setVolumeState(self: *DataPlane, vol_tag: u64, to: router.VolumeState) router.RouterError!void {
        try self.rt.setState(vol_tag, to);
        _ = self.vols.setActive(vol_tag, to == .active);
        try self.rt.refreshPlacement();
    }

    fn openVolume(self: *DataPlane, d: set_config.DataVolume, env: *const std.process.Environ.Map, opened: *u32, rt_state: router.VolumeState) OpenError!void {
        var zb: [linux.PATH_MAX]u8 = undefined;
        if (d.uri.len >= zb.len) return error.DeviceRefused;
        @memcpy(zb[0..d.uri.len], d.uri);
        zb[d.uri.len] = 0;
        const z = zb[0..d.uri.len :0];
        const dev = nvme_dev.Device.open(self.gpa, z) catch |e| {
            log.err("data volume {s}: open failed ({s})", .{ d.uri, @errorName(e) });
            return error.DeviceRefused;
        };
        errdefer dev.deinit();
        // Every wait on the device parks through the executor: a handler
        // fiber's DMA holds the fiber, never its lane thread (before the
        // first request — the probe read below is one).
        dev.routeWaits(self.io);
        // The identity through THIS device (the discovery pass read it
        // through a scratch one): tag, set, geometry, transform.
        {
            const sector = dev.probeReadBlock(0, data_superblock.sector_len) catch return error.Io;
            defer sector.release();
            const fmt = data_superblock.classifySector0(sector.bytes(), null) catch return error.IdentityMismatch;
            const sb = switch (fmt) {
                .blank => return error.IdentityMismatch,
                .v2 => |sb| sb,
            };
            if (sb.vol_tag != d.vol_tag or !std.mem.eql(u8, &sb.device_uuid, &d.device_uuid) or sb.block_size != self.block_size or sb.chunk_size != self.chunk) {
                log.err("data volume {s}: identity disagrees with the set_config record (device vol-{x:0>16} vs record vol-{x:0>16})", .{ d.uri, sb.vol_tag, d.vol_tag });
                return error.IdentityMismatch;
            }
        }
        // The allocator: index 0 is the superblock chunk (reserved forever);
        // the tail copy's chunk lies PAST the capacity so it is never minted.
        const ordinal: u16 = @intCast(opened.*);
        const capacity_bytes = (d.capacity_blocks + 1) * self.chunk;
        const alloc = block_alloc.BlockAllocator.init(self.gpa, block_alloc.BlockAllocator.configFromEnv(env, d.vol_tag, ordinal, self.chunk, capacity_bytes, false)) catch return error.OutOfMemory;
        errdefer alloc.deinit();
        alloc.recoverBlock(0);
        // The durable ledger seeds the population (refcount = references).
        var idxs: std.ArrayList(u64) = .empty;
        defer idxs.deinit(self.gpa);
        const population = block_refs.volumePopulation(self.refs_store, self.gpa, d.vol_tag, &idxs) catch |e| {
            log.err("data volume {s}: the durable reference ledger is unreadable ({s})", .{ d.uri, @errorName(e) });
            return error.Io;
        };
        if (population != null) alloc.seedFromDurableRefs(idxs.items);
        budget.authority.register(alloc.budgetComponent());
        self.rt.addVolume(.{ .id = .{ .tag = d.vol_tag }, .state = rt_state, .capacity_bytes = d.capacity_blocks * self.block_size, .used_bytes = alloc.usedBlocks() * self.block_size }) catch return error.OutOfMemory;
        self.vols.add(.{ .vol_tag = d.vol_tag, .ordinal = ordinal, .alloc = alloc, .passthrough = self.passthrough, .write_cache_volatile = dev.writeCache() == .write_back or dev.writeCache() == .unknown, .active = .init(rt_state == .active) });
        self.devs[ordinal] = dev;
        // The zcrx lane for this device, judged by the probe ladder (loud
        // once, Absent on this host unless a steerable HDS NIC fronts an
        // nvme-tcp target); published to the funnel when present.
        self.zcrx_bound[ordinal] = zcrx.arm.armVolume(self.gpa, self.io, env, &self.zcrx_registry, d.uri, derive.facts(), budget.authority.level());
        if (self.zcrx_bound[ordinal]) |b| self.zcrx_funnel.publish(ordinal, &b.lane);
        opened.* += 1;
        log.info("data volume {s}: vol-{x:0>16} bound as ordinal {d}: {d} usable blocks, {d} referenced ({d} ref records), {d} free", .{ d.uri, d.vol_tag, ordinal, d.capacity_blocks, alloc.usedBlocks() -| 1, population orelse 0, alloc.freeBlocksCount() + alloc.virginBlocks() });
    }

    fn closeVolumes(self: *DataPlane, n: u32) void {
        var i: u32 = 0;
        while (i < n) : (i += 1) {
            // The lane first: its session tears down (queues joined) before
            // the kernel initiator's device closes.
            _ = self.zcrx_funnel.retire(@intCast(i));
            if (self.zcrx_bound[i]) |b| {
                b.deinit();
                self.zcrx_bound[i] = null;
            }
            if (self.devs[i]) |dev| {
                dev.deinit();
                self.devs[i] = null;
            }
            if (self.vols.rows[i]) |v| v.alloc.deinit();
        }
        self.vols = .{};
    }

    fn openTransform(self: *DataPlane, gpa: Allocator, io: Io, cfg: *const set_config.Config, tcfg: transform.VolumeTransform, opts: Options) OpenError!void {
        var crypto: ?*transform.CryptoState = null;
        if (cfg.encryption != .none) {
            const derived = opts.derived_key orelse return error.TransformRefused;
            // Inside a rotation window the NEXT key is the wrap in force and
            // the primary the retiring arm (`rekey.zig`'s law).
            if (opts.derived_next) |next| {
                crypto = transform.CryptoState.init(gpa, io, cfg.encryption, next, derived, cfg.nonce_floor, 1) catch return error.TransformRefused;
            } else {
                crypto = transform.CryptoState.init(gpa, io, cfg.encryption, derived, null, cfg.nonce_floor, 1) catch return error.TransformRefused;
            }
        }
        errdefer if (crypto) |c| c.deinit();
        const t = transform.Transform.init(opts.env, tcfg, crypto) catch return error.TransformRefused;
        self.xform = .{ .t = t, .crypto = crypto, .codecs = undefined, .scratches = undefined };
        const x = &self.xform.?;
        // The codecs point at the transform IN the plane (a stable address
        // for the mount's life); one per executor lane.
        x.codecs = transform.CodecPool.init(gpa, &x.t, transform.codecPoolSize(self.sqz.lanes.len)) catch return error.TransformRefused;
        errdefer x.codecs.deinit(gpa);
        x.scratches = transform.ScratchPool.init(gpa, transform.scratchPoolCap(opts.facts.cpus), @intCast(transform.scratchLen(cfg.block_size))) catch return error.OutOfMemory;
        budget.authority.register(x.scratches.component());
    }

    // ---- the key-rotation faces (`ops/defrag/rekey.zig`; charter §6.5) ----

    pub const RekeyError = error{ NotEncrypted, RotationOpen, RandomFailed, OutOfMemory };

    /// Open the window in RAM: the session key re-wrapped under `next`
    /// (every header written from here carries it), then the two barriers
    /// — every codec acquired and released (every transform under the old
    /// wrap has returned; the retired wrap is zeroized behind it), then
    /// every scratch (every old-wrap image's DMA has landed — a scratch is
    /// held through the DMA that reads it). After this the sweep's census
    /// is complete by construction.
    pub fn beginRekey(self: *DataPlane, io: Io, next: transform.DerivedKey) RekeyError!void {
        const x = &(self.xform orelse return error.NotEncrypted);
        const c = x.crypto orelse return error.NotEncrypted;
        try c.beginRotation(io, next);
        try self.codecBarrier(io);
        c.retireWrap();
        try self.scratchBarrier(io);
        log.info("data plane: key-rotation window open — new writes wrap under key id {x}; the previous key unwraps only", .{&c.keyId()});
    }

    /// Close the window in RAM after the config flipped durably: the
    /// previous key's arm drops and its KEK is zeroized behind the codec
    /// barrier (no reader can hold the retired wrap past it).
    pub fn closeRekey(self: *DataPlane, io: Io) RekeyError!void {
        const x = &(self.xform orelse return error.NotEncrypted);
        const c = x.crypto orelse return error.NotEncrypted;
        c.endRotation();
        try self.codecBarrier(io);
        c.retireWrap();
        log.info("data plane: key-rotation window closed — key id {x} is the only key; the previous key is forgotten", .{&c.keyId()});
    }

    /// Hold every codec at once, then release them: every transform that
    /// held one before has returned.
    fn codecBarrier(self: *DataPlane, io: Io) error{OutOfMemory}!void {
        const x = &self.xform.?;
        const held = try self.gpa.alloc(*transform.Codec, x.codecs.capacity());
        defer self.gpa.free(held);
        for (held) |*h| h.* = x.codecs.acquire(io);
        for (held) |h| x.codecs.release(io, h);
    }

    /// Hold every scratch at once, then release them: every DMA that read
    /// an image out of one has landed.
    fn scratchBarrier(self: *DataPlane, io: Io) error{OutOfMemory}!void {
        const x = &self.xform.?;
        const held = try self.gpa.alloc(transform.ScratchPool.Buf, x.scratches.capacity());
        defer self.gpa.free(held);
        for (held) |*h| h.* = x.scratches.acquire(io);
        for (held) |h| x.scratches.release(io, h);
    }

    /// Whether the plane's crypto state has a rotation window open.
    pub fn rekeyWindowOpen(self: *DataPlane) bool {
        const x = &(self.xform orelse return false);
        const c = x.crypto orelse return false;
        return c.rotating();
    }

    /// The sweep's quiescence before its confirming census: the detached
    /// uploads and promotions done, every open pack sealed (its tenants
    /// classifiable; no old-key tenant lands in a pack opened after the
    /// barriers), no tenant between reserve and commit.
    pub fn rekeyQuiesce(self: *DataPlane, io: Io) bool {
        self.wp.quiesce(io);
        self.tasks.wait(io) catch {};
        if (self.packer) |*p| {
            p.sealAllAs(.rekey);
            return p.tenantsInFlight() == 0;
        }
        return true;
    }

    /// Which key wraps the image at `rel_off` of a block: its first header
    /// grain read and classified. `none` for a non-image (a zeroed window).
    pub fn rekeyClassify(self: *DataPlane, vol_tag: u64, block_idx: u64, rel_off: u32) rekey.Class {
        const x = &(self.xform orelse return .none);
        const c = x.crypto orelse return .none;
        const v = self.vols.byTag(vol_tag) orelse return .none;
        if (rel_off >= self.chunk) return .none;
        const page = x.scratches.acquire(self.io);
        defer x.scratches.release(self.io, page);
        const grain = page[0..transform.HEADER_PAGE_LEN];
        self.deviceRead(vol_tag, v.alloc.offsetOf(block_idx) + rel_off, grain, .write_side) catch return .none;
        const h = transform.decodeHeader(x.t.cfg, grain) catch return .none;
        return switch (c.classify(&h.key_blob)) {
            .current => .current,
            .previous => .previous,
            .foreign => .foreign,
        };
    }

    /// Every allocated block of every volume but the reserved chunk 0: the
    /// confirming census's universe (an image DMA'd before its publish is
    /// allocated and so inside it).
    pub fn rekeyAllocatedBlocks(self: *DataPlane, gpa: Allocator) Allocator.Error![]rekey.BlockKey {
        var out: std.ArrayList(rekey.BlockKey) = .empty;
        errdefer out.deinit(gpa);
        for (self.vols.rows[0..self.vols.n()]) |r| {
            const v = r.?;
            const high = v.alloc.highestBlockIndex();
            var idx: u64 = RESERVED_BLOCKS;
            while (idx <= high) : (idx += 1) {
                if (v.alloc.stateOf(idx) == .allocated) try out.append(gpa, .{ .vol_tag = v.vol_tag, .block_idx = idx });
            }
        }
        return out.toOwnedSlice(gpa);
    }

    /// The mover's rewrite hook: every window of `task` under the previous
    /// key re-encoded under the current one through a codec of this lane.
    pub fn rekeyRewrite(self: *DataPlane, task: rekey.Task, image: []u8, len: usize) error{ Io, OutOfMemory }!?usize {
        const x = &(self.xform orelse return error.Io);
        const c = x.crypto orelse return error.Io;
        const w = rekey.Windows.of(task) catch |e| {
            switch (e) {
                error.LenConflict => rekey.stats.rekey_tenant_len_conflict.fire("two referencers of one tenant window name different stored lengths"),
                error.TooManyWindows => rekey.stats.rekey_tenant_len_conflict.fire("more tenant windows than a chunk holds"),
            }
            return error.Io;
        };
        const codec = x.codecs.acquire(self.io);
        defer x.codecs.release(self.io, codec);
        return rekey.rewriteImage(self.gpa, codec, c, w, image, len) catch |e| switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.LenDrift => blk: {
                rekey.stats.rekey_image_len_drift.fire("a re-encoded image's stored length differs from the old one");
                break :blk error.Io;
            },
            error.Corrupt => error.Io,
        };
    }

    fn closeTransform(self: *DataPlane) void {
        if (self.xform) |*x| {
            x.scratches.deinit(self.gpa);
            x.codecs.deinit(self.gpa);
            if (x.crypto) |c| c.deinit();
            self.xform = null;
        }
    }

    /// The staging ring under THIS member's root of the first cache path
    /// (0003 §T.1): the root bound with its live claim (a live owner's root
    /// refused, another member's never adopted), the segment sized from the
    /// root's free space, the recovery scan seeding the pool's ledger; the
    /// other members' ownerless residue reported loud.
    fn openStaging(self: *DataPlane, gpa: Allocator, io: Io, cfg: *const set_config.Config, opts: Options) OpenError!void {
        _ = io;
        const container = cfg.cache_paths[0];
        const ours = stagingOwnerMarker(self.backend);
        var refusal: staging.Ring.BindRefusal = .{};
        var root = staging.Ring.bindMember(self.backend.pool, container, ours, &.{STAGING_SEGMENT_NAME}, &refusal) catch |e| switch (e) {
            error.OwnerLive => {
                if (refusal.owner) |o| {
                    if (o.identity() == ours.identity()) {
                        log.err("staging root {s}/{s}{x:0>8}: held LIVE by another mount of this member scope (appender {d}, node {x:0>16}, pid {d}) — two mounts present one member (mount slot {x:0>8}); refusing (never adopted): unmount the other, or mount at another mount point (a joiner's member scope derives from it)", .{ container, staging.Ring.MEMBER_DIR_PREFIX, ours.mount_slot, o.appender_id, o.node_token, o.owner_pid, ours.mount_slot });
                    } else {
                        log.err("staging root {s}/{s}{x:0>8}: held LIVE by a mount of ANOTHER SET on this cache path (staging identity {x:0>16}, appender {d}, node {x:0>16}, pid {d}) — refusing (never adopted, never wiped): give each set its own cache path (`config set-cache-paths`)", .{ container, staging.Ring.MEMBER_DIR_PREFIX, ours.mount_slot, o.identity(), o.appender_id, o.node_token, o.owner_pid });
                    }
                } else {
                    log.err("staging root {s}/{s}{x:0>8}: held LIVE by another process (no readable marker) — refusing (never adopted)", .{ container, staging.Ring.MEMBER_DIR_PREFIX, ours.mount_slot });
                }
                return error.StagingOwnerLive;
            },
            error.ForeignOwner => {
                const o = refusal.owner.?;
                log.err("staging root {s}/{s}{x:0>8}: its marker names ANOTHER member (mount slot {x:0>8}, appender {d}, node {x:0>16}, last pid {d}) — its ring is that member's custody; refusing (never adopted, never wiped)", .{ container, staging.Ring.MEMBER_DIR_PREFIX, ours.mount_slot, o.mount_slot, o.appender_id, o.node_token, o.owner_pid });
                return error.StagingForeignOwner;
            },
            else => {
                log.err("staging root under {s}: cannot bind this member's root ({s})", .{ container, @errorName(e) });
                return error.StagingRefused;
            },
        };
        errdefer root.release();
        if (root.verdict == .match) if (root.found) |f| if (f.owner_pid != ours.owner_pid or f.node_token != ours.node_token) {
            log.info("staging root {s}: this member's previous incarnation (appender {d}, node {x:0>16}, pid {d}) is dead — its ring recovers here as the successor's", .{ root.path(), f.appender_id, f.node_token, f.owner_pid });
        };
        _ = staging.Ring.residueCensus(self.backend.pool, container, ours.mount_slot, STAGING_SEGMENT_NAME);
        const capacity = stagingCapacity(root.fd, opts.budget_bytes);
        var mapped = staging.Ring.openSegment(root.fd, STAGING_SEGMENT_NAME, capacity) catch |e| {
            log.err("staging root {s}: cannot open the segment ({s})", .{ root.path(), @errorName(e) });
            return error.StagingRefused;
        };
        errdefer mapped.close();
        self.stage = .{ .root = root, .marker = ours, .mapped = mapped, .seg = undefined, .pool = undefined };
        const st = &self.stage.?;
        st.seg = staging.Segment.init(gpa, st.mapped.map, st.mapped.backing()) catch return error.OutOfMemory;
        errdefer st.seg.deinit();
        // The departure law's round: the backend's coalesced barrier (the
        // layout publish that stopped naming a record durable before the
        // record's slot is punched).
        st.seg.cover_seam = .{ .ctx = self, .run = &coverRun };
        const rep = st.seg.recover();
        st.pool = staging.Pool.init(gpa, capacity, .{ .ctx = self, .pressure = &promotionPressure, .promote = &promoteHook, .dismount = &dismountHook });
        st.pool.seedFromRecovery(&st.seg) catch return error.OutOfMemory;
        budget.authority.register(st.pool.component());
        // The live cells' value-hash states (R5: ≤ `cellStateBound(capacity)`)
        // and the segment's fixed per-cell tables (= `tablesBound(capacity)`).
        budget.authority.register(staging.cellStateComponent());
        budget.authority.register(staging.tablesComponent());
        log.info("staging ring {s}/{s}/{s}: {d} MiB, recovered {d} record(s) ({d} B), {d} torn, {d} superseded", .{ st.root.path(), staging.Ring.SEGMENT_DIR, STAGING_SEGMENT_NAME, capacity >> 20, rep.adopted, rep.bytes, rep.torn, rep.superseded });
    }

    /// The member root this plane's staging ring and NVMe tier live under;
    /// null with staging off.
    pub fn stagingRoot(self: *const DataPlane) ?[]const u8 {
        const st = &(self.stage orelse return null);
        return st.root.path();
    }

    /// `struct statfs` of the 64-bit ABIs (`fstatfs(2)`; std carries no
    /// wrapper): only `f_bsize` and `f_bavail` are read.
    const StatfsWire = extern struct {
        f_type: i64,
        f_bsize: i64,
        f_blocks: u64,
        f_bfree: u64,
        f_bavail: u64,
        f_files: u64,
        f_ffree: u64,
        f_fsid: [2]i32,
        f_namelen: i64,
        f_frsize: i64,
        f_flags: i64,
        f_spare: [4]i64,
    };

    /// `clamp(free/4, 64 MiB, budget/8)` on the 4 KiB grain (module constants).
    pub fn stagingCapacityFor(free_bytes: u64, budget_bytes: u64) u64 {
        const ceiling = @max(budget_bytes / STAGING_BUDGET_DIVISOR, STAGING_FLOOR);
        const share = std.math.clamp(free_bytes / STAGING_SHARE_DIVISOR, STAGING_FLOOR, ceiling);
        return derive.alignDown(share, staging.RECORD_ALIGN);
    }

    fn stagingCapacity(root_fd: linux.fd_t, budget_bytes: u64) u64 {
        var sfs: StatfsWire = undefined;
        const rc = linux.syscall2(.fstatfs, @as(usize, @bitCast(@as(isize, root_fd))), @intFromPtr(&sfs));
        const free: u64 = if (linux.errno(rc) == .SUCCESS and sfs.f_bsize > 0) sfs.f_bavail * @as(u64, @intCast(sfs.f_bsize)) else STAGING_FLOOR * STAGING_SHARE_DIVISOR;
        return stagingCapacityFor(free, budget_bytes);
    }

    /// The NVMe read tier under this member's root (`openStaging` bound it).
    fn openNvmeTier(self: *DataPlane, gpa: Allocator, opts: Options) nvme_read_cache.Cache {
        var aggregate: u64 = 0;
        for (self.vols.rows[0..self.vols.n()]) |r| aggregate += r.?.alloc.capacityBlocks() * self.block_size;
        const tier_budget = nvme_read_cache.budgetFromEnv(opts.env, aggregate);
        if (tier_budget == 0) return nvme_read_cache.Cache.initDisabled(gpa);
        const shards = nvme_read_cache.shardPlan(tier_budget, opts.facts.cpus, self.chunk);
        const per_shard = derive.alignDown(tier_budget / @max(shards, 1), nvme_read_cache.GRAIN);
        if (per_shard < self.chunk + nvme_read_cache.GRAIN) return nvme_read_cache.Cache.initDisabled(gpa);
        const root = self.stagingRoot().?;
        return nvme_read_cache.Ring.open(gpa, root, shards, per_shard) catch |e| {
            log.err("NVMe read tier under {s}: cannot open ({s}) — the tier is OFF", .{ root, @errorName(e) });
            return nvme_read_cache.Cache.initDisabled(gpa);
        };
    }

    fn closeStaging(self: *DataPlane) void {
        if (self.stage) |*st| {
            st.pool.deinit();
            st.seg.deinit();
            st.mapped.close();
            st.root.release();
            self.stage = null;
        }
    }

    /// The dismount pass (charter §8 `umount`, §6.1 "the writing mount's
    /// clean unmount"): the pending promotions and detached uploads drain;
    /// every staged file promotes; the open packs seal DISMOUNT; the
    /// reclaimer drains. It runs while every arm the promotion mints and
    /// frees through is still LIVE — the grant plane (the allocator's mint
    /// source), the wire's foreign-layout and free-ship legs — so `mount`
    /// calls it BEFORE the wire plane stops; `close` runs it once if nobody
    /// did (a bring-up failure's unwind). A promotion that fails leaves its
    /// resident staged (never lost: the ring is durable, the next mount
    /// adopts it) and the pass is loud about it. A resident whose ino has
    /// no live inode, or whose layout does not name it, is destroyed here
    /// (`judgeAtDismount`): the ring release is the step AFTER a promotion's
    /// or a destroy's commit, outside the transaction, so a kill between
    /// the two leaves a record nothing names — kept, it is a `[C4]` orphan
    /// once its inode is gone (THE KILL MATRIX under two rings per queue).
    pub fn dismount(self: *DataPlane) void {
        if (self.dismounted) return;
        self.dismounted = true;
        const io = self.io;
        // Background sweeps stop at their next chunk boundary (the cursor
        // is durable; the next mount adopts what is left).
        self.sweeps_stop.store(true, .release);
        self.sweep_group.await(io) catch {};
        self.tasks.wait(io) catch {};
        self.wp.quiesce(io);
        if (self.stage) |*st| {
            if (staging.promoteAllAtDismount(self.gpa, &st.seg, st.pool.hooks)) |t| {
                if (t.kept != 0) {
                    log.err("dismount promotion: {d} staged file(s) ({d} B) promoted, {d} FAILED — the failed residents stay in the staging ring (readable here, zeros elsewhere) until the next mount of this host promotes them; {d} corpse record(s) and {d} unnamed record(s) destroyed", .{ t.promoted, t.promoted_bytes, t.kept, t.corpses, t.unnamed });
                } else {
                    log.info("dismount promotion: {d} staged file(s) ({d} B) promoted, 0 failed; {d} corpse record(s) and {d} unnamed record(s) destroyed", .{ t.promoted, t.promoted_bytes, t.corpses, t.unnamed });
                }
            } else |e| log.err("dismount promotion pass: {s}", .{@errorName(e)});
            // ONE cover round for every record the pass released: the
            // publishes durable, then the slots punched (a cleanly unmounted
            // ring scans empty). A failed round leaves them held; the next
            // mount's pass destroys what nothing names.
            self.coverDeparted(io);
            // The freed grains punched: a cleanly unmounted ring scans empty.
            st.seg.sweepDirty();
            if (st.seg.departedHeld() != 0) log.err("dismount promotion: {d} released ring record(s) stay on the segment — the meta barrier that would cover their layouts failed; the next mount's pass destroys them", .{st.seg.departedHeld()});
        }
        if (self.packer) |*p| p.sealAll();
        _ = self.reclaimer.drainSync(io);
    }

    /// Close after the dismount pass: the reclaimer stops; the paths, the
    /// devices and the allocators close — every step before the backend's
    /// final checkpoint.
    pub fn close(self: *DataPlane) void {
        const io = self.io;
        self.dismount();
        self.reclaimer.closeAtDismount();
        self.reclaimer.stopAndJoin(io);
        self.lane_group.await(io) catch {};
        self.rp.deinit();
        self.wp.deinit(io);
        if (self.packer) |*p| p.deinit();
        self.closeStaging();
        self.closeTransform();
        for (self.devs[0..self.vols.n()]) |d| if (d) |dev| dev.closeAtDismount();
        self.closeVolumes(self.vols.n());
        self.zcrx_registry.deinit();
        self.reclaimer.deinit();
        self.rt.deinit();
        self.windows.deinit();
        self.sweeps.deinit(self.gpa);
        self.shape.deinit(self.gpa);
        self.view.deinit();
        self.size_epochs.deinit();
        self.publish_scratch.deinit();
        self.gpa.destroy(self);
    }

    /// The session armed: the direct-store face and the zc legs.
    pub fn armSession(self: *DataPlane, face: ?HeldStoreFace, zero_copy: bool, read_zc_serve: bool) void {
        self.held_store = face;
        self.ladder.direct_leg_armed = zero_copy and self.passthrough;
        self.ladder.fd_source_armed = zero_copy and read_zc_serve and self.rp.pool.fd != null;
        log.info("data plane: session armed — held-slot direct store {s}, direct device leg {s}, fd-source probe serve {s}", .{ if (face != null) "ON" else "off", if (self.ladder.direct_leg_armed) "ON" else "off", if (self.ladder.fd_source_armed) "ON" else "off" });
    }

    // ---- the data face (kv_fs's duck-typed `Data`) ----

    pub fn hasVolumes(self: *const DataPlane) bool {
        return self.vols.n() != 0;
    }

    pub fn stagingOn(self: *const DataPlane) bool {
        return self.stage != null;
    }

    pub fn blockSize(self: *const DataPlane) u64 {
        return self.block_size;
    }

    pub fn inlineMax(self: *const DataPlane) u64 {
        return self.inline_max;
    }

    pub fn geometry(self: *const DataPlane) layout.Geometry {
        return self.geo;
    }

    /// Inline-map entries the record cap holds: `(cap − xattr envelope −
    /// header) / entry` — past it the block-map tree crossing is owed.
    pub fn inlineMapCap(self: *const DataPlane) u32 {
        const cap = record.xattrValueCap(self.backend.nodeSize());
        const fixed = 1 + layout.LAYOUT_XATTR_NAME.len + layout.LAYOUT_HEADER_LEN + 4;
        return @intCast((cap - fixed) / layout.MAP_ENTRY_WIRE_LEN);
    }

    pub fn refsSeam(self: *DataPlane) layout.RefsSeam {
        return .{ .ctx = self, .stage_add = &refsStageAdd, .stage_release = &refsStageRelease };
    }

    pub fn isPassthrough(self: *const DataPlane) bool {
        return self.passthrough;
    }

    fn refsStageAdd(_: *anyopaque, t: *tx_mod.KvTx, op: layout.RefOp) Allocator.Error!void {
        try block_refs.stageOps(t, &.{block_refs.taken(.{ .vol_tag = op.vol_tag, .block_idx = op.block_idx, .owner_ino = op.owner_ino, .block_index = op.block_index })});
    }

    fn refsStageRelease(_: *anyopaque, t: *tx_mod.KvTx, op: layout.RefOp) Allocator.Error!void {
        try block_refs.stageOps(t, &.{block_refs.released(.{ .vol_tag = op.vol_tag, .block_idx = op.block_idx, .owner_ino = op.owner_ino, .block_index = op.block_index })});
    }

    /// The filesystem publishes an inode's shape — the INODE's size and the
    /// layout class — at every layout publish and size change; the
    /// queue-thread gates (the W1 hold gate, the shared-mode probe) read
    /// it. It is the ONE publisher of the size: the plane's own map merges
    /// know the layout's data-reach floor, which can exceed the inode's
    /// size (a shrink's zero tail), and a shape above the inode admits a
    /// Shared write that extends the file under a shared guard — two such
    /// writes race their size Δs (field overwrite, not max) and the file
    /// ends shorter than its last write.
    pub const ClaimSource = struct {
        ctx: *anyopaque,
        claimOf: *const fn (ctx: *anyopaque, ino: u64) u64,
    };

    pub fn bindClaims(self: *DataPlane, src: ?ClaimSource) void {
        self.claims = src;
    }

    pub fn noteShape(self: *DataPlane, ino: u64, size: u64, kind: layout.Kind) void {
        self.shape.publish(ino, size, kind == .striped and self.passthrough);
    }

    pub fn forgetShape(self: *DataPlane, ino: u64) void {
        lease_epoch.recallWrites(ino);
        self.shape.forget(ino);
        self.size_epochs.forget(ino);
        self.view.forget(ino);
    }

    /// A size-setting commit of `ino` is about to land (the truncate, the
    /// O_TRUNC open: under their exclusive guards, before the commit): every
    /// raise formed before this moment is stale (`sizeSetSince`).
    pub fn noteSizeSet(self: *DataPlane, ino: u64) Allocator.Error!void {
        return self.size_epochs.bump(ino);
    }

    /// Whether a size-setting commit of `ino` landed since `seen` — the
    /// epoch a write's landing stamped into its outcome (`write_admit.
    /// Outcome.size_epoch`). The write's postlude raises no size on true:
    /// the truncate clipped the bytes its end would name.
    pub fn sizeSetSince(self: *DataPlane, ino: u64, seen: u64) bool {
        return self.size_epochs.setSince(ino, seen);
    }

    /// The per-ino layout lock, level (3.7) (module doc). kv_fs takes it
    /// through the data face around its truncate / corpse transactions,
    /// BEFORE its 4a set and released after the commit landed; the plane's
    /// own sections take it inside.
    pub fn lockLayout(self: *DataPlane, io: Io, ino: u64) layout.LayoutLocks.Guard {
        // A layout RMW of the inode (every publish, any origin): its direct
        // WRITE leases are recalled before the lock (decision 0003 §AY).
        lease_epoch.recallWrites(ino);
        return self.locks.layouts.acquire(io, ino);
    }

    /// Whether a HELD slot may ride the write path unmaterialized (the
    /// session installed the direct-store face).
    pub fn canStoreHeld(self: *const DataPlane) bool {
        return self.held_store != null;
    }

    /// THE striped write: the handler's level-1 admission passed through
    /// as `class`; `file` is the layout view the caller read.
    pub fn write(self: *DataPlane, io: Io, ino: u64, offset: u64, payload: write_admit.Payload, class: op_guard.Class, file: seams.FileView, park_hook: ?write_admit.ParkHook) write_admit.Error!write_admit.Outcome {
        return self.wp.write(io, .{ .ino = ino, .offset = offset, .payload = payload, .class = class, .file = file, .park_hook = park_hook });
    }

    /// A striped truncate-up's extension law (decision 0003 §AL): the
    /// mapped EOF block's `[old_size, min(new_size, its end))` written as
    /// zeros (`write_admit.WritePath.zeroExtension`), landed by the
    /// caller's custody flush before its size commit.
    pub fn zeroExtension(self: *DataPlane, io: Io, ino: u64, old_size: u64, new_size: u64) write_admit.Error!void {
        return self.wp.zeroExtension(io, ino, .{ .kind = .striped, .size = old_size, .map_resident = true }, new_size);
    }

    /// The ordered law's floor (decision 0003 §AL.5):
    /// `write_admit.WritePath.custodyFloor`.
    pub fn custodyFloor(self: *DataPlane, io: Io, ino: u64) u64 {
        return self.wp.custodyFloor(io, ino);
    }

    /// The ordered law's data barrier for a batch of size claims
    /// (`fsync.barrierInos`).
    pub fn barrierData(self: *DataPlane, io: Io, claimants: []const u64) Error!void {
        fsync_mod.barrierInos(self.wp.fsyncCtx(), io, claimants) catch |e| return switch (e) {
            error.WriterGuardFenced => error.WriterGuardFenced,
            error.VolumeFailed => error.VolumeFailed,
            error.OutOfMemory => error.OutOfMemory,
            error.NoSpaceLeft, error.InputOutput, error.IntentBarrierFailed, error.MetaBarrierFailed => error.InputOutput,
        };
    }

    /// THE read of any layout kind into `dest` (the reply window;
    /// `registered` = the device may DMA straight into it).
    pub fn read(self: *DataPlane, io: Io, ino: u64, offset: u64, dest: []u8, registered: bool, odirect: bool) Error!usize {
        return self.rp.read(io, ino, offset, dest.len, .{ .buf = dest, .registered = registered }, .{ .odirect = odirect });
    }

    /// ONE whole-block image landed on a fresh block (the promotion to
    /// striped / the own-block arm): placement by the router, the mint, the
    /// transform, the DMA — the proof minted by the completion — and the
    /// volume's barrier over it (`promotionBarrier`: DUR-1, the block is on
    /// stable storage before any layout names it). The caller publishes the
    /// layout naming `mapping` in its own transaction and calls
    /// `abandonOwnBlock` if that fails.
    pub fn ownBlock(self: *DataPlane, io: Io, ino: u64, image: []const u8) Error!OwnBlock {
        if (image.len > self.block_size) return error.FileTooBig;
        const tag = (self.rt.pickVolume() catch return error.OutOfMemory) orelse return error.NoSpaceLeft;
        const stored = try self.transformWrite(tag, image);
        defer stored.release(self);
        return self.ownBlockStored(io, ino, tag, stored.bytes);
    }

    /// `ownBlock` from an image already transformed for `tag` (the
    /// promotion transforms ONCE to pick its arm, then lands that image).
    fn ownBlockStored(self: *DataPlane, io: Io, ino: u64, tag: u64, stored: []const u8) Error!OwnBlock {
        const v = self.vols.byTag(tag) orelse return error.InputOutput;
        const minted = enospc.allocateWithValve(v.alloc, io, enospc.Valve.fromReclaimer(self.reclaimer)) catch |e| return switch (e) {
            error.StorageFull => error.NoSpaceLeft,
            error.ReadOnly => error.ReadOnlyFileSystem,
            error.PlaneRefused, error.NotFree => error.InputOutput,
        };
        // A live owner until the caller's publish lands or the mint returns
        // (decision 0003 §AH.1): `ownBlockPublished` / `abandonOwnBlock`.
        v.alloc.holdInflight(minted.idx);
        errdefer v.alloc.dropInflight(minted.idx);
        var guard = block_alloc.MintedBlockGuard.init(v.alloc, minted);
        errdefer guard.abandon();
        if (self.test_own_block_poison) try self.poisonMint(tag, minted.offset);
        self.deviceWrite(tag, minted.offset, stored) catch |e| return mapDma(e);
        try self.promotionBarrier(io, v);
        v.alloc.publishBlock(minted.idx);
        guard.disarm();
        stats.own_block_dmas.inc();
        // The bytes the DMA landed — a transformed image's stored length,
        // grain-padded — so the device-write ledger closes (decision 0003 §AK.6).
        stats.own_block_dma_bytes.add(layout.ceilGrain(stored.len));
        self.wp.touched.stampOrdinal(ino, v.ordinal);
        // The proof names what the DMA landed: a whole-block DMA is the
        // complete payload or image by construction, so `DmaProof.covers`
        // takes it from the slot's start at any grain-padded length (a
        // sub-block image — an inline file promoting at its first write past
        // `inline_max` — and a transformed image shorter than the chunk both
        // publish; the tail of a fresh mint is unnamed and never read).
        return .{ .mapping = Mapping.wholeBlock(tag, minted.offset, minted.stamp), .proof = .{ .vol_tag = tag, .offset = minted.offset, .len = layout.ceilGrain(stored.len), .completed_ns = clock.monotonicNs() }, .alloc = v.alloc, .idx = minted.idx };
    }

    /// DUR-1 for the promotion arms (charter §6.1 "block I/O before the
    /// metadata type flip"; `fsync.BarrierCoalescer`): the volume the
    /// promotion DMA'd onto is flushed BEFORE the layout naming the block
    /// publishes, so a publish the journal's cadence makes durable never
    /// names bytes a power cut took from the device's volatile cache. One
    /// flush per batch of concurrent promotions; a write-through volume
    /// owes none.
    fn promotionBarrier(self: *DataPlane, io: Io, v: *const seams.VolumeInfo) Error!void {
        if (!v.write_cache_volatile) {
            stats.promotion_barrier_write_through_skips.inc();
            return;
        }
        stats.promotion_barriers.inc();
        return self.promotion_barriers[v.ordinal].request(io, self.deviceSeam(), v.vol_tag, &stats.promotion_barriers_coalesced);
    }

    /// `SQUEEZEFS_TEST_OWN_BLOCK_POISON`: the minted block whole as 0xAA —
    /// the previous owner's bytes the image grain lands over.
    fn poisonMint(self: *DataPlane, tag: u64, offset: u64) Error!void {
        const buf = self.gpa.alignedAlloc(u8, .fromByteUnits(layout.GRAIN), @intCast(self.block_size)) catch return error.OutOfMemory;
        defer self.gpa.free(buf);
        @memset(buf, 0xAA);
        self.deviceWrite(tag, offset, buf) catch |e| return mapDma(e);
    }

    /// The publish failed: the never-published mint returns, then its
    /// in-flight registration drops.
    pub fn abandonOwnBlock(self: *DataPlane, ob: OwnBlock) void {
        _ = self;
        ob.alloc.abandonUnpublished(ob.idx);
        ob.alloc.dropInflight(ob.idx);
    }

    /// The publish naming the own block landed: the mint's in-flight
    /// registration drops (the layout references the block now).
    pub fn ownBlockPublished(self: *DataPlane, ob: OwnBlock) void {
        _ = self;
        ob.alloc.dropInflight(ob.idx);
    }

    // @anchor:ops_jobs_republish — the ONE mover's layout primitive (rung 8).

    pub const RepublishError = error{ Superseded, InputOutput, OutOfMemory };

    /// THE MOVER's republish (charter §8 "copy-then-republish via ONE mover
    /// primitive"; `ops/lifecycle/mover.zig` step 8): under the (3.7)
    /// layout lock read the record, require the entry at `block_index` to
    /// name `expected` (same reference AND the same window — the decoration
    /// `rel_off/len` is preserved verbatim), swap it for `new` and publish
    /// in ONE tx whose named-reference diff stages `+dst −src` for this
    /// owner. A mismatch (a foreground write replaced the mapping, a
    /// truncate pruned it, a clone re-staged the tenant) is `Superseded` —
    /// the contractual no-op. The expected-binding compare under the lock
    /// is strictly stronger than a fencing token (an expected-mismatch IS
    /// the supersession witness). The caller frees the displaced source
    /// through `freeBindings` AFTER this returns (RES-1). A block-map-tree
    /// (kvmap) head is republished through the window merge's own
    /// transaction shape, which the sweep owns — refused `Superseded` here
    /// so the mover defers and the census re-plans once the window has
    /// folded (never a lost binding).
    pub fn republishBinding(self: *DataPlane, io: Io, ino: u64, block_index: u32, expected: Mapping, new: Mapping, proof: layout.DmaProof) RepublishError!void {
        const gpa = self.gpa;
        const lock = self.lockLayout(io, ino);
        defer lock.release(io);
        const rec = (self.layoutRecord(ino) catch return error.InputOutput) orelse return error.Superseded;
        defer gpa.free(rec.value);
        const prev = layout.XattrLayout.decode(rec.value, self.geo) catch return error.InputOutput;
        var entries: std.ArrayList(layout.MapEntryWire) = .empty;
        defer entries.deinit(gpa);
        const next: layout.Layout = switch (prev.kind) {
            .packed_ => blk: {
                // The tenant IS the durable copy (a packed layout carries no
                // inline map — `lookup` answers null there and read every
                // pack move as Superseded: 5,110 stale-token no-ops over a
                // 20-tenant drain before this, rung 8).
                if (block_index != 0) return error.Superseded;
                const cur = prev.durableCopy() orelse return error.Superseded;
                if (!cur.sameRef(expected, self.chunk) or cur.rel_off != expected.rel_off or cur.len != expected.len) return error.Superseded;
                break :blk layout.Layout.packedTenant(prev.size, prev.file_id, new);
            },
            .striped => blk: {
                if (prev.isKvmap()) {
                    // The block-map tree: ONE window put of the one index
                    // (`+dst −src` in the same tx, the window store's own
                    // shape — a drain moves a kvmap-mapped file block by
                    // block through it, rung 8). The sweep cursor's
                    // invariant stands: an index at/above a live cursor is
                    // the sweep's, not a mover's.
                    const head = prev.kvmapHead().?;
                    if (head.sweep) |k| if (block_index >= k) return error.Superseded;
                    const cur = (kvmap.lookup(self.mapStore(), gpa, ino, block_index, self.chunk) catch return error.InputOutput) orelse return error.Superseded;
                    if (!cur.sameRef(expected, self.chunk) or cur.rel_off != expected.rel_off or cur.len != expected.len or cur.whole_block != expected.whole_block) return error.Superseded;
                    const t = self.backend.begin(@src()) catch return error.OutOfMemory;
                    var out: seams.MergeOutcome = .{};
                    const insert = [_]seams.MapInsert{.{ .block = block_index, .binding = new }};
                    kvmap.stageWindow(self.mapStore(), gpa, t, ino, &insert, self.chunk, &out) catch |e| {
                        self.backend.abandon(t);
                        return switch (e) {
                            error.OutOfMemory => error.OutOfMemory,
                            else => error.InputOutput,
                        };
                    };
                    self.backend.commit(t) catch |e| return switch (e) {
                        error.OutOfMemory => error.OutOfMemory,
                        else => error.InputOutput,
                    };
                    self.windows.invalidate(ino);
                    stats.layout_republishes.inc();
                    return;
                }
                var found = false;
                for (prev.entries) |e| {
                    if (e.block() != block_index) {
                        try entries.append(gpa, e);
                        continue;
                    }
                    const cur = Mapping.decode(e.mapping, self.chunk) catch return error.InputOutput;
                    if (!cur.sameRef(expected, self.chunk) or cur.rel_off != expected.rel_off or cur.len != expected.len or cur.whole_block != expected.whole_block) return error.Superseded;
                    try entries.append(gpa, (layout.MapEntry{ .block = block_index, .mapping = new }).encode());
                    found = true;
                }
                if (!found) return error.Superseded;
                break :blk layout.Layout.stripedInline(prev.size, entries.items);
            },
            .inline_, .staged => return error.Superseded,
        };
        const t = self.backend.begin(@src()) catch return error.OutOfMemory;
        layout.publishLayout(t, .{ .ino = ino, .name_key = self.name_key, .coll_seq = rec.coll_seq, .prev = prev, .layout = next, .proofs = &.{proof} }, self.refsSeam(), self.geo) catch |e| {
            self.backend.abandon(t);
            return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                else => error.InputOutput,
            };
        };
        self.backend.commit(t) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => error.InputOutput,
        };
        stats.layout_republishes.inc();
    }

    /// Terminal frees AFTER a landed transaction (RES-1): displaced and
    /// destroyed bindings through the reclaimer's ladder. Each is a release
    /// its site holds in flight from before its commit until after this
    /// free (decision 0003 §AH.1) — one reaching here unheld was a window an
    /// online census read as leaked.
    pub fn freeBindings(self: *DataPlane, io: Io, bindings: []const Mapping) void {
        for (bindings) |b| {
            stats.bindings_freed.inc();
            if (self.vols.byTag(b.vol_tag)) |v| if (!v.alloc.inflightContains(v.alloc.idxOf(b.offset))) {
                stats.release_free_unheld.fire("a released binding reached its free with no in-flight registration");
            };
            // A tenant of a pack still OPEN leaves through the packer (the
            // pin arithmetic counts it); everything else runs the ladder.
            if (self.packer) |*p| if (p.releaseOpenTenant(b)) continue;
            _ = self.reclaim_free.seam().free(io, b);
        }
    }

    /// The in-flight registrations of releases on this mount's volumes
    /// (decision 0003 §AH.1): taken before a releasing commit, dropped after
    /// the frees (`dropReleases`) or on the path that abandons the release.
    pub fn releaseHolds(self: *DataPlane) seams.ReleaseHolds {
        return self.reclaim_free.holds();
    }

    pub fn holdReleases(self: *DataPlane, entries: []const layout.MapEntry) void {
        const h = self.releaseHolds();
        for (entries) |e| h.hold_fn(h.ctx, e.mapping);
    }

    pub fn dropReleases(self: *DataPlane, entries: []const layout.MapEntry) void {
        const h = self.releaseHolds();
        for (entries) |e| h.drop_fn(h.ctx, e.mapping);
    }

    /// The bindings a release frees after its commit, freed then their
    /// holds dropped (the holds were taken before the commit).
    fn freeReleased(self: *DataPlane, io: Io, bindings: []const Mapping) void {
        self.freeBindings(io, bindings);
        self.releaseHolds().drop(bindings);
    }

    /// A truncate or destroy begins: every write-side custody of `ino`
    /// lands or drops, every detached upload of the mount lands — so the
    /// layout the caller clips is the whole truth.
    pub fn flushCustody(self: *DataPlane, io: Io, ino: u64) void {
        self.wp.settleCustody(io, ino);
    }

    /// RELEASE (the no-open posture: a FLUSH that found custody, or the
    /// idle drain of an owed ino): the write-back class drains (parked
    /// blocks flush, the overlay drains, the epoch closes).
    pub fn releaseIno(self: *DataPlane, io: Io, ino: u64) void {
        self.wp.flushIno(io, ino);
    }

    /// The no-open FLUSH's try-only probe (decision 0003 §AF): whether
    /// `releaseIno` would land anything for `ino` (`WritePath.holdsCustody`).
    /// The write-back timer's drain of one ino (decision 0003 §AJ).
    pub fn drainAged(self: *DataPlane, io: Io, ino: u64, cutoff_ns: u64) write_admit.WritePath.AgedDrain {
        return self.wp.drainAged(io, ino, cutoff_ns);
    }

    pub fn hasWriteCustody(self: *DataPlane, ino: u64) bool {
        return self.wp.holdsCustody(ino);
    }

    /// fsync (charter §6.4) through `fsync.zig`'s legs — the staged leg
    /// over this ring (`staged_key` names the ino's resident record); the
    /// meta barrier is the backend's coalesced barrier.
    pub fn fsync(self: *DataPlane, io: Io, ino: u64, staged_key: ?staging.StagingKey) Error!void {
        var ctx = self.wp.fsyncCtx();
        ctx.meta_sync = .{ .ctx = self, .sync = &metaSync };
        ctx.staged_key = staged_key;
        if (self.stage) |*st| {
            ctx.promotion = st.pool.hooks;
            ctx.staged_segment = &st.seg;
        }
        var prof = fsync_mod.Prof.start();
        defer prof.finish();
        fsync_mod.fsync(ctx, io, ino, &prof) catch |e| return switch (e) {
            error.NoSpaceLeft => error.NoSpaceLeft,
            error.InputOutput, error.IntentBarrierFailed, error.MetaBarrierFailed => blk: {
                log.err("fsync ino {d}: a leg failed ({s}) — EIO to the application, the touched bits stay set", .{ ino, @errorName(e) });
                break :blk error.InputOutput;
            },
            error.WriterGuardFenced => error.WriterGuardFenced,
            error.VolumeFailed => error.VolumeFailed,
            error.OutOfMemory => error.OutOfMemory,
        };
    }

    /// syncfs (FUSE_SYNCFS) through `fsync.zig`'s mount-wide ladder: every
    /// write-side authority of this mount drained, the data barrier, the
    /// epochs' saves. The meta barrier is the filesystem's (store-wide).
    pub fn syncfs(self: *DataPlane, io: Io, prof: *fsync_mod.SyncfsProf) Error!void {
        var ctx = self.wp.fsyncCtx();
        if (self.stage) |*st| {
            ctx.promotion = st.pool.hooks;
            ctx.staged_segment = &st.seg;
        }
        fsync_mod.syncfs(ctx, self.gpa, io, prof) catch |e| return switch (e) {
            error.NoSpaceLeft => error.NoSpaceLeft,
            error.InputOutput, error.IntentBarrierFailed, error.MetaBarrierFailed => blk: {
                log.err("syncfs: a leg failed ({s}) — EIO to the application, the custody it could not land stays parked", .{@errorName(e)});
                break :blk error.InputOutput;
            },
            error.WriterGuardFenced => error.WriterGuardFenced,
            error.VolumeFailed => error.VolumeFailed,
            error.OutOfMemory => error.OutOfMemory,
        };
    }

    fn metaSync(ctx: ?*anyopaque, _: Io, _: u64) anyerror!void {
        const self: *DataPlane = @ptrCast(@alignCast(ctx.?));
        try self.backend.fsync();
    }

    /// `df` (charter §5.5 "a holder's `df` used count is its bitmap
    /// population minus its own unconsumed window"): on a grant-armed
    /// allocator `reachableFreeBlocks` is the CLEAR population of the
    /// holder's bitmap plus this writer's own window (a joiner reads the
    /// population the last grant reply carried, 0002 row 241); unarmed it
    /// is the free list, the virgin tail and the blocks inside open trim
    /// windows — all supply, never a deficit. Block 0 (the superblock
    /// chunk) is outside the capacity shown.
    pub fn statfs(self: *DataPlane) ?Space {
        if (self.vols.n() == 0) return null;
        var total: u64 = 0;
        var free: u64 = 0;
        for (self.vols.rows[0..self.vols.n()]) |r| {
            const a = r.?.alloc;
            const blocks = a.capacityBlocks() - 1;
            total += blocks * self.block_size;
            free += @min(a.reachableFreeBlocks(), blocks) * self.block_size;
        }
        return .{ .total_bytes = total, .free_bytes = free };
    }

    // ---- staging (the ring) ----

    /// A fresh staged identity: the ino and a per-mount monotone word (the
    /// ring key is `file_id ‖ ino`; the generation makes the scan exact).
    pub fn mintFileId(self: *DataPlane, ino: u64) layout.FileId {
        var f: layout.FileId = undefined;
        std.mem.writeInt(u64, f[0..8], ino, .little);
        std.mem.writeInt(u64, f[8..16], self.file_id_seq.fetchAdd(1, .monotonic), .little);
        return f;
    }

    /// Stage the WHOLE payload under `key` (admission, placement, ledger);
    /// the generation the layout record's publish is gated on. `slack` =
    /// `.append` when the write that stages it EXTENDS the image (§J.2: the
    /// slot reserves a quarter of the image so the next appends land in
    /// place).
    pub fn stageImage(self: *DataPlane, key_: staging.StagingKey, image: []const u8, slack: staging.Slack) Error!u64 {
        const st = &(self.stage orelse return error.NotSupported);
        const s = staging.stageWrite(&st.pool, &st.seg, key_, image, 1, slack) catch |e| return switch (e) {
            error.StorageFull => error.NoSpaceLeft,
            error.OutOfMemory => error.OutOfMemory,
        };
        // The lazy reclaim's trigger: an eighth of the ring freed and not
        // yet punched ⇒ one sweep task (`staging.Segment.sweepDirty`).
        if (staging.stats.staging_dirty_free_bytes.load() >= st.seg.capacity() / RING_SWEEP_DIVISOR and !st.sweep_armed.swap(true, .acq_rel)) {
            self.tasks.spawn(self.io, sweepTask, .{self}) catch st.sweep_armed.store(false, .release);
        }
        return s.generation;
    }

    /// The dirty-free share of the ring that arms a sweep: an eighth — a
    /// few hundred punches a second at the arm's rate, each over a whole
    /// lap of freed records, instead of one per re-stage.
    const RING_SWEEP_DIVISOR: u64 = 8;

    fn sweepTask(self: *DataPlane) Io.Cancelable!void {
        const st = &(self.stage orelse return);
        st.seg.sweepDirty();
        st.sweep_armed.store(false, .release);
    }

    /// §J.2: an extending write lands IN PLACE in the resident record's
    /// slack (`data` at value offset `at ≥ val_len`); the new generation, or
    /// null when the caller must re-stage the whole image (no ring, no
    /// resident copy, no slack for it).
    pub fn appendStaged(self: *DataPlane, key_: staging.StagingKey, at: u64, data: []const u8) Error!?u64 {
        const st = &(self.stage orelse return null);
        const s = (staging.appendStaged(&st.pool, &st.seg, key_, at, data, 1) catch return error.OutOfMemory) orelse return null;
        return s.generation;
    }

    /// The staged arm's fast-path view of `ino` (the read view: no KV
    /// lookup when it holds the ino current): its identity and the size
    /// readers see (the parked claim folded); null = not a staged file, or
    /// a foreign slot. A staged layout belongs to a regular file by
    /// construction.
    pub const StagedPoint = struct { size: u64, file_id: layout.FileId };

    /// The cached point alone (no lookup on a miss): the write's first
    /// question, asked of every write — a striped write pays two atomic
    /// loads, never a record read (the fill through `resolveAt` on every
    /// write cost the striped rows 6–13 % and thrashed the read view).
    pub fn stagedPointCached(self: *DataPlane, ino: u64) ?StagedPoint {
        if (self.stage == null) return null;
        const hit = self.staged_points.lookup(ino, ino_witness.genOf(ino), self.view.movesNow()) orelse {
            stats.staged_point_misses.inc();
            return null;
        };
        stats.staged_point_hits.inc();
        return .{ .size = self.claimedSize(ino, hit.size), .file_id = hit.file_id };
    }

    /// The point through the read view and the store, cached on the way
    /// out (the staged arm's slow path warms it for the next write).
    pub fn stagedPoint(self: *DataPlane, ino: u64) Error!?StagedPoint {
        if (self.stage == null) return null;
        // The witness and the move epoch BEFORE the lookups (the read
        // view's law): a record that lands meanwhile moves the witness past
        // what the entry carries.
        const gen = ino_witness.genOf(ino);
        const moves = self.view.movesNow();
        const r = (try self.resolveAt(ino, null)) orelse return null;
        const p = r.point orelse return null;
        if (p.kind != .staged) return null;
        const size = r.size orelse return null;
        if (!self.backend.isReader()) self.staged_points.fill(ino, gen, moves, size, p.file_id);
        return .{ .size = self.claimedSize(ino, size), .file_id = p.file_id };
    }

    /// The staged fast path's per-ino cache: the read view's validation law
    /// (the ino witness's generation and the slot-move epoch read before
    /// the fill's lookups; a seqlock per entry, every word an acquire load,
    /// the second seq load after them) over a payload of one cache line —
    /// the view's 4 × 64 slots thrash under a thousand hot small files
    /// (`read_view_misses` 5.3 M to 18 k hits on the 16-job row), and the
    /// write asks for an ino's identity and size alone.
    const StagedPointCache = struct {
        const N: usize = 4096;
        const W_SEQ = 0;
        const W_INO = 1;
        const W_GEN = 2;
        const W_MOVES = 3;
        const W_SIZE = 4;
        const W_FID_LO = 5;
        const W_FID_HI = 6;
        const Entry = struct { w: [8]Atomic(u64) align(std.atomic.cache_line) };
        const Hit = struct { size: u64, file_id: layout.FileId };

        entries: [N]Entry = @splat(.{ .w = @splat(.init(0)) }),

        fn slot(self: *StagedPointCache, ino: u64) *Entry {
            return &self.entries[@intCast(dlm.splitmix64(ino) & (N - 1))];
        }

        fn lookup(self: *StagedPointCache, ino: u64, gen: u64, moves: u64) ?Hit {
            const e = self.slot(ino);
            const s0 = e.w[W_SEQ].load(.acquire);
            if (s0 & 1 != 0 or s0 == 0) return null;
            if (e.w[W_INO].load(.acquire) != ino or e.w[W_GEN].load(.acquire) != gen or e.w[W_MOVES].load(.acquire) != moves) return null;
            const size = e.w[W_SIZE].load(.acquire);
            const lo = e.w[W_FID_LO].load(.acquire);
            const hi = e.w[W_FID_HI].load(.acquire);
            if (e.w[W_SEQ].load(.acquire) != s0) return null;
            var fid: layout.FileId = undefined;
            std.mem.writeInt(u64, fid[0..8], lo, .little);
            std.mem.writeInt(u64, fid[8..16], hi, .little);
            return .{ .size = size, .file_id = fid };
        }

        /// A racing filler loses (the entry is a cache; the loser's words
        /// would be as good).
        fn fill(self: *StagedPointCache, ino: u64, gen: u64, moves: u64, size: u64, file_id: layout.FileId) void {
            const e = self.slot(ino);
            const s0 = e.w[W_SEQ].load(.acquire);
            if (s0 & 1 != 0) return;
            if (e.w[W_SEQ].cmpxchgStrong(s0, s0 + 1, .acq_rel, .monotonic) != null) return;
            e.w[W_INO].store(ino, .release);
            e.w[W_GEN].store(gen, .release);
            e.w[W_MOVES].store(moves, .release);
            e.w[W_SIZE].store(size, .release);
            e.w[W_FID_LO].store(std.mem.readInt(u64, file_id[0..8], .little), .release);
            e.w[W_FID_HI].store(std.mem.readInt(u64, file_id[8..16], .little), .release);
            e.w[W_SEQ].store(s0 + 2, .release);
        }
    };

    /// The in-place overwrite inside one grain of the resident record
    /// (`staging.overwriteStaged`): the new generation, or null when the
    /// caller must re-stage (no ring, no resident copy, a synced record, a
    /// span outside one grain).
    pub fn overwriteStaged(self: *DataPlane, io: Io, key_: staging.StagingKey, at: u64, data: []const u8) Error!?u64 {
        const st = &(self.stage orelse return null);
        const s = (staging.overwriteStaged(&st.pool, &st.seg, io, key_, at, data, 1) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.InputOutput => error.InputOutput,
            error.NoSpaceLeft => error.NoSpaceLeft,
        }) orelse return null;
        return s.generation;
    }

    /// The ring entry leaves (a promotion landed, a re-stage superseded it,
    /// an unlink, a layout that moved away): generation-gated when known
    /// (null = whatever copy is resident — the layout no longer names it).
    /// The filesystem's face, on a handler fiber: the copies depart
    /// (`staging.Segment.remove`) and a cover round runs on a task of its
    /// own — the caller's commit is acked already and owes no barrier wait.
    pub fn releaseStaged(self: *DataPlane, key_: staging.StagingKey, generation: ?u64) void {
        const st = &(self.stage orelse return);
        const departed = if (generation) |g| staging.releaseStaged(&st.pool, &st.seg, key_, g) else blk: {
            const removed = st.seg.remove(key_, null);
            if (removed) _ = st.pool.credit(key_.file_id, null);
            break :blk removed;
        };
        if (!departed) return;
        self.tasks.spawn(self.io, coverTask, .{self}) catch self.coverDeparted(self.io);
    }

    fn coverTask(self: *DataPlane) Io.Cancelable!void {
        self.coverDeparted(self.io);
    }

    /// ONE cover round (`staging.Segment.cover`): the departed ring copies
    /// whose publish the meta barrier made durable are released. A failed
    /// round is counted; the copies stay held for the next.
    fn coverDeparted(self: *DataPlane, io: Io) void {
        const st = &(self.stage orelse return);
        st.seg.cover(io) catch {
            stats.promotion_cover_rounds_failed.inc();
        };
    }

    fn coverRun(ctx: ?*anyopaque, _: Io) staging.DurableError!void {
        const self: *DataPlane = @ptrCast(@alignCast(ctx.?));
        self.backend.fsync() catch return error.InputOutput;
    }

    /// This client's ring image (the RMW source of a re-stage); the lease
    /// pins the cell until released.
    pub fn stagedLease(self: *DataPlane, key_: staging.StagingKey) ?staging.Segment.Lease {
        const st = &(self.stage orelse return null);
        return st.seg.get(key_);
    }

    /// The whole image a mapping names (a packed tenant / an own block):
    /// the re-stage source. `out.len ≥ ceil(len)`.
    pub fn readMappingImage(self: *DataPlane, m: Mapping, out: []u8) Error![]const u8 {
        const w = m.readWindow(self.chunk, null) catch return error.InputOutput;
        if (w.len > out.len) return error.BufferTooSmall;
        const v = self.vols.byTag(w.vol_tag) orelse return error.InputOutput;
        const before = v.alloc.fillIncarnation(v.alloc.idxOf(m.offset)) orelse return error.InputOutput;
        try self.deviceRead(w.vol_tag, w.device_off, out[0..@intCast(w.len)], .write_side);
        if (!v.alloc.fillIncarnationStill(v.alloc.idxOf(m.offset), before)) return error.InputOutput;
        if (self.xform != null) {
            // A tenant image is a transform frame: decode into a pooled
            // scratch, then the plaintext view copied back into `out`.
            const x = &self.xform.?;
            const scratch = x.scratches.acquire(self.io);
            defer x.scratches.release(self.io, scratch);
            const plain = self.decodeStored(out[0..@intCast(w.len)], scratch) catch return error.InputOutput;
            if (plain.len > out.len) return error.BufferTooSmall;
            @memcpy(out[0..plain.len], plain);
            return out[0..plain.len];
        }
        return out[w.lo..w.hi];
    }

    /// The PLAINTEXT of a transformed mapping into `out`, a buffer the
    /// image's plaintext fits (the write side's block-sized seed: the
    /// seeded checkout's cell, the flush's complement). The stored window —
    /// the chunk, `block + 4 KiB`, for a whole-block mapping — lands in ONE
    /// pooled scratch and decodes straight into `out` (decision 0003 §AK:
    /// read into `out` itself, a whole-block window never fit and every
    /// write into a mapped transformed block answered EIO).
    fn readMappingPlain(self: *DataPlane, m: Mapping, out: []u8) Error![]const u8 {
        const x = &self.xform.?;
        const w = m.readWindow(self.chunk, null) catch return error.InputOutput;
        const raw = x.scratches.acquire(self.io);
        defer x.scratches.release(self.io, raw);
        if (w.len > raw.len) return error.BufferTooSmall;
        const v = self.vols.byTag(w.vol_tag) orelse return error.InputOutput;
        const before = v.alloc.fillIncarnation(v.alloc.idxOf(m.offset)) orelse return error.InputOutput;
        try self.deviceRead(w.vol_tag, w.device_off, raw[0..@intCast(w.len)], .write_side);
        if (!v.alloc.fillIncarnationStill(v.alloc.idxOf(m.offset), before)) return error.InputOutput;
        return self.decodeStored(raw[0..@intCast(w.len)], out) catch return error.InputOutput;
    }

    // ---- the promotion primitive (staging → inline / own block / packed) ----

    fn promotionPressure(ctx: ?*anyopaque, k: staging.StagingKey) void {
        const self: *DataPlane = @ptrCast(@alignCast(ctx.?));
        if (k.ino() == 0) return;
        self.tasks.spawn(self.io, promoteTask, .{ self, k }) catch {};
    }

    fn promoteTask(self: *DataPlane, k: staging.StagingKey) Io.Cancelable!void {
        _ = self.promoteStaged(k);
    }

    fn promoteHook(ctx: ?*anyopaque, k: staging.StagingKey) bool {
        const self: *DataPlane = @ptrCast(@alignCast(ctx.?));
        return self.promoteStaged(k);
    }

    fn dismountHook(ctx: ?*anyopaque, k: staging.StagingKey) staging.DismountVerdict {
        const self: *DataPlane = @ptrCast(@alignCast(ctx.?));
        return self.judgeAtDismount(k);
    }

    /// The ONE primitive (charter §6.1): dispatch on size — fits inline ⇒
    /// inline; `slot > CHUNK/2` ⇒ its own block; else a packed tenant. The
    /// identity is re-checked under the promotion's locks — the (3.5) stripe
    /// THEN the (3.7) layout lock (module doc; `PublishLocks.lockPromotion`)
    /// — (still staged, same file id, same ring generation); true iff the
    /// entry left the ring. A displaced durable copy is released after the
    /// locks drop (RES-1). A record the layout does not name stays: the
    /// live points run beside the ops (an unlinked-open file reads its
    /// custody until its FORGET); the dismount pass judges it.
    pub fn promoteStaged(self: *DataPlane, k: staging.StagingKey) bool {
        return self.promoteUnder(k, .live) == .promoted;
    }

    /// The dismount pass's verdict on one resident (`staging.DismountVerdict`;
    /// charter §6.1 "a corpse's custody is destroyed, never promoted"): the
    /// ino's inode read under the promotion's locks — absent or `nlink == 0`
    /// ⇒ `.corpse`; then the promotion — a record the layout does not name
    /// (no layout, kind not staged, another file id) ⇒ `.unnamed`; a
    /// resource decline ⇒ `.kept`. A corpse's or an unnamed record leaves
    /// the ring (generation-gated, the pool credited). A spilled block
    /// (`writeback.blockKey`) is write custody no layout names by design:
    /// judged by its ino alone, kept while the ino lives.
    pub fn judgeAtDismount(self: *DataPlane, k: staging.StagingKey) staging.DismountVerdict {
        return self.promoteUnder(k, .dismount);
    }

    const PromotionPass = enum { live, dismount };

    fn promoteUnder(self: *DataPlane, k: staging.StagingKey, pass: PromotionPass) staging.DismountVerdict {
        const io = self.io;
        const st = &(self.stage orelse return .kept);
        const ino = k.ino();
        stats.promotions_run.inc();
        const lease = st.seg.get(k) orelse {
            stats.promotions_stale.inc();
            return .kept;
        };
        defer lease.release();
        const generation = lease.generation();
        const image = lease.bytes();
        const locks = self.locks.lockPromotion(io, ino);
        const corpse = pass == .dismount and self.inoIsCorpse(ino);
        const out: Promoted = if (corpse) .{ .ok = false } else self.promoteLocked(io, ino, k, generation, image);
        const verdict: staging.DismountVerdict = if (out.ok) .promoted else if (corpse) .corpse else if (pass == .dismount and out.unnamed and writeback.blockOfKey(k) == null) .unnamed else .kept;
        if (verdict == .corpse or verdict == .unnamed) {
            log.debug("dismount promotion ino {d}: {s} record destroyed ({d} B)", .{ ino, @tagName(verdict), image.len });
            _ = staging.releaseStaged(&st.pool, &st.seg, k, generation);
        }
        locks.release(io);
        if (out.displaced) |d| {
            if (self.packer) |*p| p.releaseTenantRef(d);
            self.releaseHolds().drop(&.{d});
        }
        // The departed record's cover round, after the locks (the meta
        // barrier is a park); the dismount pass runs ONE round for all of
        // its residents (`dismount`).
        if (pass == .live and verdict != .kept) self.coverDeparted(io);
        return verdict;
    }

    /// Under the promotion's locks: the ino has no live inode record. A
    /// LOCAL ino's record is read here; a foreign slot's ino is its
    /// holder's — `.gone` at the holder is a corpse, a held token is judged
    /// by its carried layout alone (never this mount's projection). A read
    /// that fails is no verdict (the record stays).
    fn inoIsCorpse(self: *DataPlane, ino: u64) bool {
        if (self.foreign_layout) |f| switch (f.lookup(f.ctx, self.gpa, ino) catch return false) {
            .local => {},
            .gone => return true,
            .foreign => |x| {
                if (x.layout) |v| self.gpa.free(v);
                return false;
            },
        };
        const v = (self.inodeRecord(ino) catch return false) orelse return true;
        return v.nlink == 0;
    }

    /// `unnamed`: the layout does not name this record (none, kind not
    /// staged, another file id) — residue to the dismount pass; a
    /// generation race or a resource decline is not.
    const Promoted = struct { ok: bool, displaced: ?Mapping = null, unnamed: bool = false };

    fn promoteLocked(self: *DataPlane, io: Io, ino: u64, k: staging.StagingKey, generation: u64, image: []const u8) Promoted {
        const st = &self.stage.?;
        // Every decline names itself at debug level: the dismount pass counts
        // them as FAILED, and a count without a cause is not evidence.
        const cur_bytes = (self.layoutRecord(ino) catch |e| {
            log.debug("promotion ino {d}: the layout record failed to read ({t})", .{ ino, e });
            return .{ .ok = false };
        }) orelse {
            stats.promotions_stale.inc();
            log.debug("promotion ino {d}: stale — no layout record (the file is gone; the ring entry is an orphan)", .{ino});
            return .{ .ok = false, .unnamed = true };
        };
        defer self.gpa.free(cur_bytes.value);
        const cur = layout.XattrLayout.decode(cur_bytes.value, self.geo) catch |e| {
            log.debug("promotion ino {d}: the layout record failed to decode ({t})", .{ ino, e });
            return .{ .ok = false };
        };
        if (cur.kind != .staged or !std.mem.eql(u8, &cur.file_id, &k.file_id)) {
            stats.promotions_stale.inc();
            log.debug("promotion ino {d}: stale — layout kind {t}, file id {s}: the layout does not name this record", .{ ino, cur.kind, if (std.mem.eql(u8, &cur.file_id, &k.file_id)) "same" else "DIFFERENT" });
            return .{ .ok = false, .unnamed = true };
        }
        if (st.seg.residentGeneration(k) != generation) {
            stats.promotions_stale.inc();
            log.debug("promotion ino {d}: stale — resident generation {?d} vs leased {d}", .{ ino, st.seg.residentGeneration(k), generation });
            return .{ .ok = false };
        }
        const policy = if (self.packer) |*p| p.policy else pack.Policy{ .enabled = false, .max_slot = self.chunk / 2 };
        // ONE transform per promotion: the stored image decides the arm and
        // is the image the own-block / packed arm lands.
        const tag = (self.rt.pickVolume() catch return .{ .ok = false }) orelse {
            log.debug("promotion ino {d}: no data volume to promote onto", .{ino});
            return .{ .ok = false };
        };
        const stored = self.transformWrite(tag, image) catch |e| {
            log.debug("promotion ino {d}: the transform refused ({t})", .{ ino, e });
            return .{ .ok = false };
        };
        defer stored.release(self);
        const stored_len: u64 = stored.bytes.len;
        const d = pack.dispatch(policy, cur.size, stored_len, self.inline_max, self.chunk) catch |e| {
            stats.promotions_failed.inc();
            log.debug("promotion ino {d}: no arm for size {d} / stored {d} ({t})", .{ ino, cur.size, stored_len, e });
            return .{ .ok = false };
        };
        const out: Promoted = switch (d) {
            .inline_ => .{ .ok = self.promoteInline(ino, cur, cur_bytes.coll_seq, image, k, generation) },
            .own_block => .{ .ok = self.promoteOwnBlock(io, ino, cur, cur_bytes.coll_seq, tag, stored.bytes, k, generation) },
            .packed_ => blk: {
                // The durable copy the pack commit displaces is a live owner
                // until `promoteStaged` releases it (decision 0003 §AH.1).
                const copy = cur.durableCopy();
                if (copy) |m| self.releaseHolds().hold(&.{m});
                const p = self.promotePacked(io, ino, cur_bytes.coll_seq, tag, stored.bytes, k, generation);
                if (copy) |m| if (p.displaced == null) self.releaseHolds().drop(&.{m});
                break :blk p;
            },
        };
        if (out.ok) self.noteShape(ino, cur.size, if (d == .own_block) .striped else if (d == .inline_) .inline_ else .packed_) else {
            stats.promotions_failed.inc();
            log.debug("promotion ino {d}: the {t} arm declined (size {d}, stored {d})", .{ ino, d, cur.size, stored_len });
        }
        return out;
    }

    fn promoteInline(self: *DataPlane, ino: u64, prev: layout.Layout, coll_seq: u8, image: []const u8, k: staging.StagingKey, generation: u64) bool {
        const data = image[0..@intCast(@min(image.len, prev.size))];
        const t = self.backend.begin(@src()) catch return false;
        layout.publishLayout(t, .{ .ino = ino, .name_key = self.name_key, .coll_seq = coll_seq, .prev = prev, .layout = layout.Layout.inlineData(prev.size, data), .proofs = &.{} }, self.refsSeam(), self.geo) catch {
            self.backend.abandon(t);
            return false;
        };
        self.backend.commit(t) catch return false;
        _ = staging.releaseStaged(&self.stage.?.pool, &self.stage.?.seg, k, generation);
        pack.stats.layout_promoted_inline.inc();
        return true;
    }

    fn promoteOwnBlock(self: *DataPlane, io: Io, ino: u64, prev: layout.Layout, coll_seq: u8, tag: u64, stored: []const u8, k: staging.StagingKey, generation: u64) bool {
        const ob = self.ownBlockStored(io, ino, tag, stored) catch |e| {
            log.debug("promotion ino {d}: own block refused ({t})", .{ ino, e });
            return false;
        };
        const entries = [_]layout.MapEntryWire{(layout.MapEntry{ .block = 0, .mapping = ob.mapping }).encode()};
        const t = self.backend.begin(@src()) catch {
            self.abandonOwnBlock(ob);
            return false;
        };
        layout.publishLayout(t, .{ .ino = ino, .name_key = self.name_key, .coll_seq = coll_seq, .prev = prev, .layout = layout.Layout.stripedInline(prev.size, &entries), .proofs = &.{ob.proof} }, self.refsSeam(), self.geo) catch {
            self.backend.abandon(t);
            self.abandonOwnBlock(ob);
            return false;
        };
        self.backend.commit(t) catch {
            self.abandonOwnBlock(ob);
            return false;
        };
        self.ownBlockPublished(ob);
        _ = staging.releaseStaged(&self.stage.?.pool, &self.stage.?.seg, k, generation);
        pack.stats.layout_promoted_block.inc();
        return true;
    }

    /// The packed arm under the caller's (3.5) + (3.7): `pack.commitLocked`
    /// re-checks the identity and commits; its displaced copy rides back
    /// for the release after the locks drop.
    fn promotePacked(self: *DataPlane, io: Io, ino: u64, coll_seq: u8, tag: u64, stored: []const u8, k: staging.StagingKey, generation: u64) Promoted {
        const p = &(self.packer orelse return .{ .ok = false });
        const st = &self.stage.?;
        const prepared = (p.prepare(io, .{ .vol_tag = tag, .scope = self.scope }, stored, clock.monotonicNs()) catch |e| {
            log.debug("promotion ino {d}: pack prepare refused ({t})", .{ ino, e });
            return .{ .ok = false };
        }) orelse {
            log.debug("promotion ino {d}: the pack arm is stopped", .{ino});
            return .{ .ok = false };
        };
        // DUR-1: the tenant's DMA on stable storage before the packed
        // layout naming it publishes.
        if (self.vols.byTag(tag)) |v| self.promotionBarrier(io, v) catch |e| {
            p.abandonPrepared(prepared);
            log.debug("promotion ino {d}: the pack arm's barrier failed ({t})", .{ ino, e });
            return .{ .ok = false };
        };
        const t = self.backend.begin(@src()) catch {
            p.abandonPrepared(prepared);
            return .{ .ok = false };
        };
        const outcome = p.commitLocked(prepared, .{
            .ino = ino,
            .name_key = self.name_key,
            .coll_seq = coll_seq,
            .file_id = k.file_id,
            .generation = generation,
            .lookup = .{ .ctx = self, .current = &packLookupCurrent },
            .refs = self.refsSeam(),
            .commit = .{ .ctx = self, .commit = &packCommit },
            .pool = &st.pool,
            .seg = &st.seg,
            .geo = self.geo,
        }, t);
        return switch (outcome) {
            .committed => |displaced| .{ .ok = true, .displaced = displaced },
            .stale => blk: {
                self.backend.abandon(t);
                log.debug("promotion ino {d}: the pack commit answered stale", .{ino});
                break :blk .{ .ok = false };
            },
            .failed => |why| blk: {
                // A failed COMMIT already consumed the tx (`KvBackend.commit`'s
                // contract); only a failed publish leaves it ours to abandon.
                if (why == .publish) self.backend.abandon(t);
                log.debug("promotion ino {d}: the pack commit answered failed ({t})", .{ ino, why });
                break :blk .{ .ok = false };
            },
        };
    }

    /// The packer's "current layout" (borrowed until the next call on this
    /// thread — the record bytes live in a per-thread buffer sized to the
    /// record cap).
    fn packLookupCurrent(ctx: *anyopaque, ino: u64) ?layout.Layout {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const S = struct {
            threadlocal var buf: [record.RECORD_VALUE_CAP_CEILING]u8 = undefined;
        };
        const rec = (self.layoutRecord(ino) catch return null) orelse return null;
        defer self.gpa.free(rec.value);
        if (rec.value.len > S.buf.len) return null;
        @memcpy(S.buf[0..rec.value.len], rec.value);
        return layout.XattrLayout.decode(S.buf[0..rec.value.len], self.geo) catch null;
    }

    fn packCommit(ctx: *anyopaque, t: *tx_mod.KvTx) pack.CommitError!void {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        self.backend.commit(t) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => error.CommitFailed,
        };
    }

    fn packAllocSeam(self: *DataPlane) pack.AllocSeam {
        return .{ .ctx = self, .allocate_placed = &packAllocatePlaced, .pin = &packPin, .release = &packRelease, .publish = &packPublish, .abandon = &packAbandon, .refcount = &packRefcount };
    }

    fn packAllocatePlaced(ctx: *anyopaque, k: pack.PackKey) pack.AllocError!pack.Mint {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const v = self.vols.byTag(k.vol_tag) orelse return error.VolumeFailed;
        const m = enospc.allocateWithValve(v.alloc, self.io, enospc.Valve.fromReclaimer(self.reclaimer)) catch |e| return switch (e) {
            error.StorageFull => error.StorageFull,
            error.ReadOnly, error.PlaneRefused, error.NotFree => error.VolumeFailed,
        };
        return .{ .offset = m.offset, .incarnation = m.stamp };
    }

    fn packPin(ctx: *anyopaque, vol_tag: u64, offset: u64) bool {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const v = self.vols.byTag(vol_tag) orelse return false;
        return v.alloc.incrementRefcount(v.alloc.idxOf(offset));
    }

    fn packRelease(ctx: *anyopaque, vol_tag: u64, offset: u64) pack.ReleaseVerdict {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const v = self.vols.byTag(vol_tag) orelse return .untracked;
        const idx = v.alloc.idxOf(offset);
        return switch (self.reclaimer.freeBlock(self.io, @intCast(v.ordinal), idx, v.alloc.stampOf(idx))) {
            .terminal => .terminal,
            .nonterminal => .nonterminal,
            .refused => .untracked,
        };
    }

    fn packPublish(ctx: *anyopaque, vol_tag: u64, offset: u64) void {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const v = self.vols.byTag(vol_tag) orelse return;
        v.alloc.publishBlock(v.alloc.idxOf(offset));
    }

    fn packAbandon(ctx: *anyopaque, vol_tag: u64, offset: u64) void {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const v = self.vols.byTag(vol_tag) orelse return;
        v.alloc.abandonUnpublished(v.alloc.idxOf(offset));
    }

    fn packRefcount(ctx: *anyopaque, vol_tag: u64, offset: u64) u32 {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const v = self.vols.byTag(vol_tag) orelse return 0;
        return v.alloc.refcount(v.alloc.idxOf(offset)) orelse 0;
    }

    fn packReadSeam(self: *DataPlane) pack.ReadSeam {
        return .{ .ctx = self, .read_range = &packReadRange };
    }

    fn packReadRange(ctx: *anyopaque, vol_tag: u64, device_off: u64, _: u64, out: []u8) pack.ReadError!void {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        self.deviceRead(vol_tag, device_off, out, .pack) catch |e| return switch (e) {
            error.QueueFull => error.QueueFull,
            error.WorkerShutdown => error.WorkerShutdown,
            error.OutOfMemory => error.OutOfMemory,
            else => error.InputOutput,
        };
    }

    // ---- the device seam (write path, overlay, patch, fsync, pack) ----

    fn deviceOf(self: *DataPlane, vol_tag: u64) ?*nvme_dev.Device {
        const v = self.vols.byTag(vol_tag) orelse {
            stats.unresolved_volume.fire("a device op named a volume tag no mounted volume carries");
            return null;
        };
        return self.devs[v.ordinal];
    }

    pub fn deviceSeam(self: *DataPlane) seams.DeviceSeam {
        return .{ .ctx = self, .vtable = &device_vtable };
    }

    const device_vtable: seams.DeviceSeam.VTable = .{ .write = &devWrite, .write_fixed = &devWriteFixed, .read = &devRead, .flush = &devFlush };

    fn mapDma(e: nvme_dev.DeviceError) seams.DmaError {
        return switch (e) {
            error.NoSpaceLeft => error.NoSpaceLeft,
            error.QueueFull, error.LaneQueueFull => error.QueueFull,
            error.WorkerShutdown, error.WorkerClosed, error.LaneStopped => error.WorkerShutdown,
            error.VolumeFailed => error.VolumeFailed,
            error.WriterGuardFenced, error.ReservationConflict, error.DismountRefused => error.WriterGuardFenced,
            error.OutOfMemory => error.OutOfMemory,
            else => error.InputOutput,
        };
    }

    fn deviceWrite(self: *DataPlane, vol_tag: u64, offset: u64, data: []const u8) nvme_dev.DeviceError!void {
        const dev = self.deviceOf(vol_tag) orelse return error.VolumeFailed;
        try dev.writeBlock(offset, data);
    }

    fn devWrite(ctx: *anyopaque, vol_tag: u64, offset: u64, data: []const u8) seams.DmaError!void {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        self.deviceWrite(vol_tag, offset, data) catch |e| return mapDma(e);
    }

    /// `WRITE_FIXED(device ← slot)` through the session's face: the held
    /// payload lands from the app's pages with zero daemon passes.
    fn devWriteFixed(ctx: *anyopaque, vol_tag: u64, offset: u64, src: seams.HeldSource) seams.DmaError!void {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const face = self.held_store orelse return error.WorkerShutdown;
        const dev = self.deviceOf(vol_tag) orelse return error.VolumeFailed;
        face.store(face.ctx, src, dev.directFd(), offset, self.io) catch |e| {
            stats.held_store_failures.inc();
            return switch (e) {
                error.SessionClosed => error.WorkerShutdown,
                error.SlotNotHeld, error.BridgeFailed => error.InputOutput,
            };
        };
        stats.held_stores.inc();
        stats.held_store_bytes.add(src.len);
    }

    /// Who asked for a device read — the ledger arm its bytes land in. The
    /// read path counts its own purpose arms (`read_path.stats.read_dma_*`)
    /// at its DMA sites; the other three are counted here.
    const ReadPurpose = enum { read_path, write_side, compose, pack };

    /// Any window: aligned straight into `out`, else bounced through a
    /// pooled read of the enclosing grain window. THE ONE data-plane read
    /// choke point: every DMA'd byte lands in `data_device_read_bytes`.
    fn deviceRead(self: *DataPlane, vol_tag: u64, offset: u64, out: []u8, purpose: ReadPurpose) Error!void {
        const dev = self.deviceOf(vol_tag) orelse return error.VolumeFailed;
        if (out.len == 0) return;
        var moved: u64 = out.len;
        if (derive.isAligned(offset, layout.GRAIN) and derive.isAligned(out.len, layout.GRAIN) and derive.isAligned(@intFromPtr(out.ptr), layout.GRAIN)) {
            dev.readInto(offset, out) catch |e| return mapRead(e);
        } else {
            stats.device_read_bounces.inc();
            const start = derive.alignDown(offset, layout.GRAIN);
            const end = derive.alignUp(offset + out.len, layout.GRAIN);
            const buf = dev.readBlock(start, @intCast(end - start)) catch |e| return mapRead(e);
            defer buf.release();
            @memcpy(out, buf.bytes()[@intCast(offset - start)..][0..out.len]);
            moved = end - start;
        }
        stats.data_device_read_bytes.add(moved);
        switch (purpose) {
            .read_path => {},
            .write_side => stats.write_side_read_dma_bytes.add(moved),
            .compose => stats.compose_read_dma_bytes.add(moved),
            .pack => stats.pack_read_dma_bytes.add(moved),
        }
    }

    fn mapRead(e: nvme_dev.DeviceError) Error {
        return switch (e) {
            error.ShortRead => error.ShortRead,
            error.QueueFull, error.LaneQueueFull => error.QueueFull,
            error.WorkerShutdown, error.WorkerClosed, error.LaneStopped => error.WorkerShutdown,
            error.VolumeFailed => error.VolumeFailed,
            error.OutOfMemory => error.OutOfMemory,
            else => error.InputOutput,
        };
    }

    fn devRead(ctx: *anyopaque, vol_tag: u64, offset: u64, out: []u8) seams.ReadError!void {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        self.deviceRead(vol_tag, offset, out, .write_side) catch |e| return switch (e) {
            error.ShortRead => error.ShortRead,
            error.QueueFull => error.QueueFull,
            error.WorkerShutdown => error.WorkerShutdown,
            error.VolumeFailed => error.VolumeFailed,
            error.OutOfMemory => error.OutOfMemory,
            else => error.InputOutput,
        };
    }

    fn devFlush(ctx: *anyopaque, vol_tag: u64) seams.DmaError!void {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const dev = self.deviceOf(vol_tag) orelse return error.VolumeFailed;
        dev.flush() catch |e| return mapDma(e);
    }

    /// The pack's DMA seam (the same device write; the proof minted by the
    /// seam's completion).
    fn dmaSeam(self: *DataPlane) layout.DmaSeam {
        return .{ .ctx = self, .write_fn = &dmaWrite };
    }

    fn dmaWrite(ctx: *anyopaque, vol_tag: u64, offset: u64, data: []const u8) layout.DmaError!void {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        self.deviceWrite(vol_tag, offset, data) catch |e| return mapDma(e);
    }

    // ---- the transform seams ----

    /// A stored image in a pooled scratch, held until `release` — across
    /// the DMA that lands it (the own-block act, the pack prepare). On a
    /// passthrough set the bytes are `plain` itself and nothing is held.
    pub const Stored = struct {
        bytes: []const u8,
        scratch: ?transform.ScratchPool.Buf,

        pub fn release(self: Stored, plane: *DataPlane) void {
            if (self.scratch) |b| plane.xform.?.scratches.release(plane.io, b);
        }
    };

    /// The stored image of `plain`: a codec of this lane's own over a
    /// pooled scratch (no plane-wide lock).
    pub fn transformWrite(self: *DataPlane, vol_tag: u64, plain: []const u8) Error!Stored {
        _ = vol_tag;
        const x = &(self.xform orelse return .{ .bytes = plain, .scratch = null });
        const scratch = x.scratches.acquire(self.io);
        errdefer x.scratches.release(self.io, scratch);
        const codec = x.codecs.acquire(self.io);
        defer x.codecs.release(self.io, codec);
        const bytes = codec.processWrite(plain, scratch) catch return error.InputOutput;
        return .{ .bytes = bytes, .scratch = scratch };
    }

    /// The plaintext of a stored `image` into `out` (≥ `scratchLen`) through
    /// a pooled codec: the plane's whole-image decode (fsck's scrub face,
    /// `readMappingImage`).
    pub fn decodeStored(self: *DataPlane, image: []u8, out: []u8) error{Corrupt}![]const u8 {
        const x = &self.xform.?;
        const codec = x.codecs.acquire(self.io);
        defer x.codecs.release(self.io, codec);
        return codec.processRead(image, out) catch return error.Corrupt;
    }

    fn transformSeam(self: *DataPlane) accumulate.TransformSeam {
        if (self.xform == null) return accumulate.TransformSeam.passthrough;
        return .{ .ctx = self, .process_write = &seamProcessWrite, .scratch_len = &seamScratchLen };
    }

    /// `seams.ImageSeam` on a transformed set: the write side's seeds read
    /// the block's PLAINTEXT image (`readMappingPlain` decodes the frame),
    /// never the raw frame bytes.
    fn imageSeamRead(ctx: ?*anyopaque, b: Mapping, out: []u8) seams.ReadError!void {
        const self: *DataPlane = @ptrCast(@alignCast(ctx.?));
        const plain = self.readMappingPlain(b, out) catch |e| return switch (e) {
            error.ShortRead => error.ShortRead,
            error.QueueFull => error.QueueFull,
            error.WorkerShutdown => error.WorkerShutdown,
            error.VolumeFailed => error.VolumeFailed,
            error.OutOfMemory => error.OutOfMemory,
            else => error.InputOutput,
        };
        // A frame that decoded short (a truncate-clipped image) reads zeros past its end.
        if (plain.len < out.len) @memset(out[plain.len..], 0);
    }

    fn seamProcessWrite(ctx: ?*anyopaque, _: u64, plain: []const u8, scratch: []u8) anyerror![]const u8 {
        const self: *DataPlane = @ptrCast(@alignCast(ctx.?));
        const x = &self.xform.?;
        const codec = x.codecs.acquire(self.io);
        defer x.codecs.release(self.io, codec);
        return codec.processWrite(plain, scratch);
    }

    fn seamScratchLen(ctx: ?*anyopaque, _: u64) usize {
        const self: *DataPlane = @ptrCast(@alignCast(ctx.?));
        return @intCast(transform.scratchLen(@intCast(self.block_size)));
    }

    fn decodeImage(ctx: *anyopaque, image: []u8, out: []u8) error{Corrupt}![]const u8 {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        return self.decodeStored(image, out);
    }

    /// The framed sub-block read's two seams (decision 0003 §K7): the span
    /// is pure over the header page; the frame decode runs on a codec of
    /// this lane's own (§K10: no plane-wide lock).
    fn decodeFrameSpan(ctx: *anyopaque, page: []const u8, rel_s: u64, rel_e: u64) error{Corrupt}!read_path.FrameSpan {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const x = &self.xform.?;
        const h = transform.decodeHeader(x.t.cfg, page) catch return error.Corrupt;
        const t = transform.decodeTable(x.t.cfg, h, page) catch return error.Corrupt;
        return transform.frameSpan(h, t, rel_s, rel_e) catch return error.Corrupt;
    }

    fn decodeRange(ctx: *anyopaque, page: []const u8, window: []u8, window_off: u64, span: read_path.FrameSpan, out: []u8) error{Corrupt}![]const u8 {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const x = &self.xform.?;
        const codec = x.codecs.acquire(self.io);
        defer x.codecs.release(self.io, codec);
        return codec.decodeFrames(page, window, window_off, span, out) catch return error.Corrupt;
    }

    // ---- the block-map authority (seams.MapSeam) ----

    pub fn mapSeam(self: *DataPlane) seams.MapSeam {
        return .{ .ctx = self, .vtable = &map_vtable };
    }

    const map_vtable: seams.MapSeam.VTable = .{ .lookup = &mapLookup, .view = &mapView, .merge = &mapMerge, .current_fencing = &mapFencing, .size_epoch = &mapSizeEpoch };

    /// The layout record of `ino`: the xattr chain at the layout name's
    /// hash, the record named `layout` (owned bytes) and its chain slot.
    /// A FOREIGN slot's record comes under its holder's token (charter
    /// §5.5: tokens are the ONLY foreign-read method; the grant carries
    /// the carried xattrs, `layout` among them), never off this mount's
    /// projection.
    const LayoutRecord = struct { value: []u8, coll_seq: u8 };

    pub fn layoutRecord(self: *DataPlane, ino: u64) Error!?LayoutRecord {
        return self.layoutRecordWith(self.gpa, ino);
    }

    /// `layoutRecord` with the copy owned by `a` (the process allocator,
    /// or a merge's slab — `merge_scratch`).
    pub fn layoutRecordWith(self: *DataPlane, a: Allocator, ino: u64) Error!?LayoutRecord {
        if (self.foreign_layout) |f| switch (try f.lookup(f.ctx, a, ino)) {
            .local => {},
            .gone => return null,
            // A token page's chain slot is the holder's; a foreign record
            // is read, never republished from here (the publish ships).
            .foreign => |x| return if (x.layout) |v| .{ .value = v, .coll_seq = 0 } else null,
        };
        const h = key.xattrNameHash56(layout.LAYOUT_XATTR_NAME, self.name_key);
        // The chain's first slot holds the record unless another name of
        // the same 56-bit hash took it first (or took it and was removed):
        // ONE copy off the borrowed view answers the common case (the
        // store's own copy was a second one); the chain range (a list, a
        // copy of every key and value, then this record's copy again) only
        // when slot 0 is not `layout`. The read path resolves a layout per
        // cold read (`docs/evidence/rung-10-dev-perf.md` §3.4).
        const Probe = union(enum) { copied: []u8, other_name, corrupt, oom };
        const Copy = struct {
            a: Allocator,
            fn visit(q: @This(), v: []const u8) Probe {
                const xv = record.XattrValue.decode(v) catch return .corrupt;
                if (!std.mem.eql(u8, xv.name, layout.LAYOUT_XATTR_NAME)) return .other_name;
                return .{ .copied = q.a.dupe(u8, v) catch return .oom };
            }
        };
        switch ((self.backend.lookupView(a, key.TREE_XATTRS, &key.xattrKey(ino, h, 0), Probe, Copy{ .a = a }, Copy.visit) catch |e| return KvBackend.coreError(e)) orelse .other_name) {
            .copied => |v| return .{ .value = v, .coll_seq = 0 },
            .other_name => {},
            .corrupt => return error.InputOutput,
            .oom => return error.OutOfMemory,
        }
        return self.layoutChain(a, ino, h);
    }

    /// The layout record off its xattr chain when slot 0 is not `layout`.
    fn layoutChain(self: *DataPlane, a: Allocator, ino: u64, h: u64) Error!?LayoutRecord {
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(self.gpa, &kvs);
        const start = key.xattrKey(ino, h, 0);
        const end = key.xattrKey(ino, h, std.math.maxInt(u8));
        self.backend.range(self.gpa, key.TREE_XATTRS, &start, &end, std.math.maxInt(u8) + 1, &kvs) catch |e| return KvBackend.coreError(e);
        for (kvs.items) |kv| {
            const xv = record.XattrValue.decode(kv.value) catch return error.InputOutput;
            if (std.mem.eql(u8, xv.name, layout.LAYOUT_XATTR_NAME)) {
                return .{ .value = try a.dupe(u8, kv.value), .coll_seq = kv.key[kv.key.len - 1] };
            }
        }
        return null;
    }

    /// The `layoutPoint` visitor over a borrowed (or owned) layout record;
    /// with a `fill` the record (or its absence) also goes into the read
    /// view under the generations the caller captured before its lookups.
    const PointProbe = union(enum) { point: layout.Point, other_name, corrupt };
    const ViewFill = struct { size: u64, gen: u64, moves: u64 };
    const PointQuery = struct {
        plane: *DataPlane,
        ino: u64,
        block: ?u32,
        fill: ?ViewFill,
        fn visit(q: @This(), v: []const u8) PointProbe {
            const xv = record.XattrValue.decode(v) catch return .corrupt;
            if (!std.mem.eql(u8, xv.name, layout.LAYOUT_XATTR_NAME)) return .other_name;
            const p = layout.resolvePoint(xv.value, q.plane.geo, q.block) catch return .corrupt;
            if (q.fill) |f| q.plane.view.fill(q.ino, f.size, xv.value, f.gen, f.moves);
            return .{ .point = p };
        }
    };

    /// What one op needs of `ino`: the inode record's size (null = no
    /// inode; the parked claim folded by `claimedSize`) and the layout as
    /// `layout.resolvePoint` reads it for `block` (null = no record: an
    /// inline file never written). Served off the read view when it holds
    /// the ino current (`read_view.zig`: no KV lookup, no divert ask);
    /// else the divert's verdict, then the two store lookups off the node
    /// cache's borrowed records (`docs/evidence/rung-10-layoutlookup.md`),
    /// the view filled on the way out of a local lookup under the witness
    /// generation and slot-move epoch read BEFORE them.
    const Resolved = struct { size: ?u64, point: ?layout.Point };

    fn resolveAt(self: *DataPlane, ino: u64, block: ?u32) Error!?Resolved {
        if (self.view.lookup(ino, block)) |v| return .{ .size = v.size, .point = v.point };
        if (self.foreign_layout) |f| switch (try f.lookup(f.ctx, self.gpa, ino)) {
            .local => {},
            .gone => return null,
            // A foreign record is the holder's, owned (`layoutRecord`): never cached here.
            .foreign => |x| return .{ .size = x.size, .point = if (x.layout) |v| try self.foreignPoint(v, block) else null },
        };
        const gen = ino_witness.genOf(ino);
        const moves = self.view.movesNow();
        const size = try self.inodeRecordSize(ino);
        // An inode-less layout is a destroy in flight: resolved, not cached.
        // A `-o ro` reader's projection moves without a leaf apply (the
        // checkpoint it adopts), so it is never cached either.
        const fill: ?ViewFill = if (size != null and !self.backend.isReader()) .{ .size = size.?, .gen = gen, .moves = moves } else null;
        const point = try self.layoutPointLocal(ino, block, fill);
        return .{ .size = size, .point = point };
    }

    /// `layoutPoint` of a record the token page carried (owned; freed here).
    fn foreignPoint(self: *DataPlane, owned: []u8, block: ?u32) Error!?layout.Point {
        defer self.gpa.free(owned);
        return switch (PointQuery.visit(.{ .plane = self, .ino = 0, .block = block, .fill = null }, owned)) {
            .point => |p| p,
            .other_name, .corrupt => error.InputOutput,
        };
    }

    /// The layout point of this mount's own slot (the foreign verdict
    /// already `local`), off the borrowed record at the chain's slot 0 or
    /// the owned chain copy; `fill` installs what it read into the view.
    fn layoutPointLocal(self: *DataPlane, ino: u64, block: ?u32, fill: ?ViewFill) Error!?layout.Point {
        const q: PointQuery = .{ .plane = self, .ino = ino, .block = block, .fill = fill };
        const h = key.xattrNameHash56(layout.LAYOUT_XATTR_NAME, self.name_key);
        switch ((self.backend.lookupView(self.gpa, key.TREE_XATTRS, &key.xattrKey(ino, h, 0), PointProbe, q, PointQuery.visit) catch |e| return KvBackend.coreError(e)) orelse .other_name) {
            .point => |p| return p,
            .corrupt => return error.InputOutput,
            .other_name => {},
        }
        const owned = ((try self.layoutChain(self.gpa, ino, h)) orelse {
            if (fill) |f| self.view.fill(ino, f.size, null, f.gen, f.moves);
            return null;
        }).value;
        defer self.gpa.free(owned);
        return switch (PointQuery.visit(q, owned)) {
            .point => |p| p,
            .other_name, .corrupt => error.InputOutput,
        };
    }

    /// `layoutPoint`: the layout alone (the write side's map seams).
    fn layoutPoint(self: *DataPlane, ino: u64, block: ?u32) Error!?layout.Point {
        return ((try self.resolveAt(ino, block)) orelse return null).point;
    }

    /// The inode's size as readers and growers see it: the record's under
    /// the parked size claim (decision 0003 §AL.5); null = no inode.
    fn inodeSize(self: *DataPlane, ino: u64) Error!?u64 {
        const r = (try self.resolveAt(ino, null)) orelse return null;
        return self.claimedSize(ino, r.size orelse return null);
    }

    fn claimedSize(self: *DataPlane, ino: u64, size: u64) u64 {
        return @max(size, if (self.claims) |c| c.claimOf(c.ctx, ino) else 0);
    }

    /// The inode record's size off the borrowed record (unfolded).
    fn inodeRecordSize(self: *DataPlane, ino: u64) Error!?u64 {
        const Size = struct {
            fn of(_: void, bytes: []const u8) ?u64 {
                return (record.InodeValue.decode(bytes) catch return null).size;
            }
        };
        const size = (self.backend.lookupView(self.gpa, key.TREE_INODES, &key.inodeKey(ino), ?u64, {}, Size.of) catch |e| return KvBackend.coreError(e)) orelse return null;
        return size orelse error.InputOutput;
    }

    /// The mount's slot-move epoch into the view (decision 0003 §X.1): a
    /// lease transition stales every view filled before it.
    pub fn bindMoves(self: *DataPlane, m: *const sym_moves.Moves) void {
        self.view.bindMoves(&m.epoch);
    }

    fn mapLookup(ctx: *anyopaque, ino: u64, block: u32) ?Mapping {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const p = (self.layoutPoint(ino, block) catch return null) orelse return null;
        if (p.kind != .striped) return null;
        return if (p.kvmap) |head| self.kvmapLookup(ino, head, block) else p.binding;
    }

    /// The tree-mapped resolve ladder: an index at or above a live sweep
    /// cursor is a HOLE (shadowed residue); then the window store's span
    /// verdict (a filled span's miss is a definitive hole); then the tree —
    /// ONE range page filled into the store. An unresolvable record is loud
    /// (`kvmap_resolve_failures`), never a fabricated hole.
    fn kvmapLookup(self: *DataPlane, ino: u64, head: layout.Layout.KvmapHead, block: u32) ?Mapping {
        if (head.sweep) |k| if (block >= k) return null;
        if (self.windows.probe(ino, block)) |p| return switch (p) {
            .hole => null,
            .mapping => |m| m,
        };
        var recs: std.ArrayList(kvmap.Rec) = .empty;
        defer recs.deinit(self.gpa);
        kvmap.rangeCovering(self.mapStore(), self.gpa, ino, block, kvmap.WINDOW_FILL_RECORDS, self.chunk, &recs) catch |e| {
            stats.kvmap_resolve_failures.fire("a tree-mapped binding could not be resolved");
            log.err("ino {d} block {d}: the block-map tree could not be read ({s})", .{ ino, block, @errorName(e) });
            return null;
        };
        var keyed_from: usize = 0;
        var found: ?Mapping = null;
        for (recs.items) |r| {
            if (r.index >= block) keyed_from += 1;
            if (r.covers(block)) found = r.bindingAt(block, self.chunk);
        }
        self.windows.fill(ino, block, recs.items, keyed_from < kvmap.WINDOW_FILL_RECORDS) catch {};
        return found;
    }

    fn mapView(ctx: *anyopaque, ino: u64) ?seams.FileView {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const r = (self.resolveAt(ino, null) catch return null) orelse return null;
        const size = self.claimedSize(ino, r.size orelse return null);
        const l = r.point orelse return .{ .kind = .inline_, .size = size, .map_resident = true };
        // A tree-mapped head resolves through `kvmapLookup` as the inline
        // map does through the record: the map is resident either way.
        return .{ .kind = switch (l.kind) {
            .inline_ => .inline_,
            .staged => .staged,
            .packed_ => .packed_,
            .striped => .striped,
        }, .size = size, .map_resident = true };
    }

    fn mapStore(self: *DataPlane) kvmap.Store {
        return kvmap.Store.from(KvBackend, self.backend);
    }

    /// Blocks a size spans (the map's index space is `offset / block_size`).
    fn blocksOf(self: *const DataPlane, size: u64) u64 {
        return (size + self.block_size - 1) / self.block_size;
    }

    fn mapKvErr(e: kvmap.Error) seams.MergeError {
        return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io, error.Corrupt => error.Transient,
        };
    }

    fn mapFencing(_: *anyopaque, _: u64) u64 {
        // Rung 4: one writer, one generation.
        return 1;
    }

    fn mapSizeEpoch(ctx: *anyopaque, ino: u64) u64 {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        return self.size_epochs.current(ino);
    }

    /// The inline map fold (pure): the current ascending entries and the
    /// inserts — in the CALLER's order (a rewrite epoch's save lists its
    /// shadowed blocks as they were fed) — folded into one ascending map,
    /// an insert replacing a block's old binding (displaced into `out`
    /// when it names another reference).
    fn foldInserts(gpa: Allocator, old_entries: []const layout.MapEntryWire, req_inserts: []const seams.MapInsert, chunk: u64, merged: *std.ArrayList(layout.MapEntryWire), out: *seams.MergeOutcome) error{ OutOfMemory, Corrupt }!void {
        const inserts = try gpa.dupe(seams.MapInsert, req_inserts);
        defer gpa.free(inserts);
        std.mem.sort(seams.MapInsert, inserts, {}, struct {
            fn lessThan(_: void, a: seams.MapInsert, b: seams.MapInsert) bool {
                return a.block < b.block;
            }
        }.lessThan);
        try merged.ensureTotalCapacity(gpa, old_entries.len + inserts.len);
        var i: usize = 0;
        var j: usize = 0;
        while (i < old_entries.len or j < inserts.len) {
            if (j == inserts.len or (i < old_entries.len and old_entries[i].block() < inserts[j].block)) {
                merged.appendAssumeCapacity(old_entries[i]);
                i += 1;
            } else if (i == old_entries.len or inserts[j].block < old_entries[i].block()) {
                merged.appendAssumeCapacity((layout.MapEntry{ .block = inserts[j].block, .mapping = inserts[j].binding }).encode());
                j += 1;
            } else {
                const old = Mapping.decode(old_entries[i].mapping, chunk) catch return error.Corrupt;
                // One displaced per insert, at most `max_displaced` inserts
                // per merge (`MergeRequest.inserts`): the else cannot run.
                if (!old.sameRef(inserts[j].binding, chunk)) {
                    if (out.n_displaced < seams.MergeOutcome.max_displaced) {
                        out.displaced[out.n_displaced] = old;
                        out.n_displaced += 1;
                    } else stats.layout_map_merge_displaced_overflow.fire("an inline-map fold displaced more bindings than MergeOutcome holds; the excess is never freed");
                }
                merged.appendAssumeCapacity((layout.MapEntry{ .block = inserts[j].block, .mapping = inserts[j].binding }).encode());
                i += 1;
                j += 1;
            }
        }
    }

    /// ONE transaction under the layout lock: the current record →
    /// the inserts folded into the inline map (ascending, replacing a
    /// block's old binding) → `publishLayout` with the `+ref`/`−ref` diff
    /// → the conveyor commit. The displaced bindings are the caller's to
    /// free after this returns (RES-1).
    ///
    /// The stale rule first (`MergeRequest.size_epoch`): a unit formed
    /// before a size-setting commit of the ino inserts only the blocks
    /// still below the ino's CURRENT size (the record folded with its
    /// parked claim — a grow since the truncate keeps its blocks) and
    /// raises no size; the inserts at or past EOF come back `clipped` for
    /// the caller to free. The truncate's own settle lands or joins every
    /// RAM custody before its clip, so this reaches only what the settle
    /// cannot see: a spilled unit waiting in the write-back queue.
    fn mapMerge(ctx: *anyopaque, req_in: seams.MergeRequest) seams.MergeError!seams.MergeOutcome {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const io = self.io;
        if (req_in.fencing != 1) return error.FencingTokenExpired;
        const lock = self.lockLayout(io, req_in.ino);
        defer lock.release(io);
        var req = req_in;
        var kept: [seams.MergeOutcome.max_displaced]seams.MapInsert = undefined;
        var clipped: u8 = 0;
        if (req_in.size_epoch) |seen| if (self.size_epochs.setSince(req_in.ino, seen)) {
            const cur = (self.inodeSize(req_in.ino) catch return error.Transient) orelse return error.NotFound;
            var n: usize = 0;
            for (req_in.inserts) |ins| {
                if (@as(u64, ins.block) * self.block_size < cur) {
                    kept[n] = ins;
                    n += 1;
                } else clipped += 1;
            }
            req.inserts = kept[0..n];
            req.min_size = 0;
            if (clipped != 0) stats.layout_map_merge_clipped_blocks.add(clipped);
            if (n == 0) return .{ .clipped = clipped };
        };
        var out = try self.mergeLocked(req);
        out.clipped = clipped;
        return out;
    }

    fn mergeLocked(self: *DataPlane, req: seams.MergeRequest) seams.MergeError!seams.MergeOutcome {
        const gpa = self.gpa;
        // A FOREIGN-slot file (the record under its holder's token): the
        // merge is computed here and the publish SHIPS to the holder.
        var foreign = false;
        if (self.foreign_layout) |f| switch (f.lookup(f.ctx, gpa, req.ino) catch return error.Transient) {
            .local => {},
            .gone => return error.NotFound,
            .foreign => |x| {
                if (x.layout) |v| gpa.free(v);
                foreign = true;
            },
        };
        // The publish's working set is ONE pooled slab (`merge_scratch`):
        // the record's copy and the fold below (both within the slab by
        // its sizing — never freed, the slab resets when it comes home),
        // then the tx's arena, which takes the slab over at `beginBacked`
        // and returns it at the tx's terminal outcome on the journal lane.
        // Past that handoff this fiber touches the slab's allocator no more.
        const slab = self.publish_scratch.acquire();
        var slab_to_tx = false;
        defer if (!slab_to_tx) slab.release();
        const sa = slab.allocator();
        const rec = (self.layoutRecordWith(sa, req.ino) catch return error.Transient);
        var prev: ?layout.Layout = null;
        var coll_seq: u8 = 0;
        if (rec) |r| {
            prev = layout.XattrLayout.decode(r.value, self.geo) catch return error.Transient;
            coll_seq = r.coll_seq;
            // A binding for a file that is no longer striped belongs to a
            // dead generation (a truncate to zero re-classed it): discard.
            if (prev.?.kind != .striped) return error.NotFound;
            if (prev.?.isKvmap()) {
                if (foreign) return self.refuseForeignKvmap(req.ino, "a tree-mapped head");
                return self.kvmapMerge(req, prev.?, coll_seq);
            }
        } else {
            // The inode must still exist: a destroyed ino takes no map.
            if ((self.inodeSize(req.ino) catch return error.Transient) == null) return error.NotFound;
        }
        const old_entries: []const layout.MapEntryWire = if (prev) |p| p.entries else &.{};
        var merged: std.ArrayList(layout.MapEntryWire) = .empty;
        var out: seams.MergeOutcome = .{};
        foldInserts(sa, old_entries, req.inserts, self.chunk, &merged, &out) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Corrupt => error.Transient,
        };
        if (merged.items.len > self.inlineMapCap()) {
            if (foreign) return self.refuseForeignKvmap(req.ino, "the inline cap's crossing");
            return self.crossing(req, prev.?, coll_seq, merged.items, &out);
        }
        const size = @max(if (prev) |p| p.size else 0, req.min_size);
        const new = layout.Layout.stripedInline(size, merged.items);
        if (foreign) {
            try self.shipMerge(req, new);
            return out;
        }
        const t = self.backend.beginBacked(@src(), sa, slab.backing()) catch return error.OutOfMemory;
        slab_to_tx = true;
        slab.primeArena(&t.arena);
        layout.publishLayout(t, .{ .ino = req.ino, .name_key = self.name_key, .coll_seq = coll_seq, .prev = prev, .layout = new, .proofs = req.proofs }, self.refsSeam(), self.geo) catch |e| {
            self.backend.abandon(t);
            stats.layout_map_merge_failures.inc();
            log.err("ino {d}: the block-map publish of {d} binding(s) failed ({s}) — the upload keeps its custody and retries", .{ req.ino, req.inserts.len, @errorName(e) });
            return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                else => error.Transient,
            };
        };
        // The displaced bindings are live owners until the caller's frees
        // (decision 0003 §AH.1).
        const holds = self.releaseHolds();
        holds.hold(out.slice());
        self.backend.commit(t) catch |e| {
            holds.drop(out.slice());
            stats.layout_map_merge_failures.inc();
            log.err("ino {d}: the block-map commit of {d} binding(s) failed ({s})", .{ req.ino, req.inserts.len, @errorName(e) });
            return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.VolumeFailed, error.VolumeClosed => error.WriterGuardFenced,
                else => error.Transient,
            };
        };
        out.holds = holds;
        stats.layout_map_merges.inc();
        stats.layout_map_merge_blocks.add(req.inserts.len);
        // The shape is NOT published here (fstests generic/013: `write_
        // shared_class_violation` ×60 per round): `size` is the layout's
        // data-reach floor — the striped shrink's zero tail lands bytes
        // past the new EOF and its upload's `min_size` is the old reach —
        // never the inode's size the shared-mode classifier admits on.
        // The filesystem publishes the inode's size at its postlude.
        return out;
    }

    /// The foreign arm of `mapMerge`: the new layout record and the
    /// window's proofs as ONE `record_ship` publish to the holder (which
    /// stages the record and its `±ref` diff in ITS slot tree and recalls
    /// the file's tokens — this writer's included).
    fn shipMerge(self: *DataPlane, req: seams.MergeRequest, new: layout.Layout) seams.MergeError!void {
        const gpa = self.gpa;
        const f = self.foreign_publish orelse {
            stats.layout_map_merge_failures.inc();
            log.err("ino {d}: the block-map publish of {d} binding(s) names a slot another appender leases and no publish ship is bound", .{ req.ino, req.inserts.len });
            return error.Transient;
        };
        if (req.proofs.len > verbs.PUBLISH_PROOFS_MAX) return error.Transient;
        const value = new.encodeAlloc(gpa) catch return error.OutOfMemory;
        defer gpa.free(value);
        var proofs: [verbs.PUBLISH_PROOFS_MAX]verbs.DmaProofWire = undefined;
        for (req.proofs, 0..) |p, i| proofs[i] = .{ .vol_tag = p.vol_tag, .offset = p.offset, .len = p.len, .completed_ns = p.completed_ns };
        f.ship(f.ctx, route_table.globalOfEffective(req.ino), value, proofs[0..req.proofs.len]) catch |e| {
            stats.layout_map_merge_failures.inc();
            log.warn("ino {d}: the shipped block-map publish of {d} binding(s) did not land ({s}) — the upload keeps its custody and retries", .{ req.ino, req.inserts.len, @errorName(e) });
            return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                else => error.Transient,
            };
        };
        stats.layout_map_merges.inc();
        stats.layout_map_merges_shipped.inc();
        stats.layout_map_merge_blocks.add(req.inserts.len);
    }

    /// The filesystem's promotion of a FOREIGN-slot file (the inline /
    /// staged / packed image on its own block, the striped layout naming
    /// it): the same ship as a merge — the holder stages the record and
    /// its `+ref`.
    pub fn publishForeign(self: *DataPlane, _: Io, ino: u64, l: layout.Layout, proofs: []const layout.DmaProof) errors.Error!void {
        const f = self.foreign_publish orelse return error.HolderUnreachable;
        if (proofs.len > verbs.PUBLISH_PROOFS_MAX) return error.InvalidArgument;
        const value = try l.encodeAlloc(self.gpa);
        defer self.gpa.free(value);
        var wire: [verbs.PUBLISH_PROOFS_MAX]verbs.DmaProofWire = undefined;
        for (proofs, 0..) |p, i| wire[i] = .{ .vol_tag = p.vol_tag, .offset = p.offset, .len = p.len, .completed_ns = p.completed_ns };
        try f.ship(f.ctx, route_table.globalOfEffective(ino), value, wire[0..proofs.len]);
        stats.layout_map_merges_shipped.inc();
    }

    /// A tree-mapped shape on a FOREIGN file: refused, counted and named
    /// — its records are the holder's slot tree's (the upload retries;
    /// the file is written from its holder).
    fn refuseForeignKvmap(self: *DataPlane, ino: u64, what: []const u8) seams.MergeError!seams.MergeOutcome {
        _ = self;
        stats.foreign_publish_kvmap_refusals.inc();
        stats.layout_map_merge_failures.inc();
        log.err("ino {d}: {s} on a file whose slot another appender leases is refused — a block-map tree is written from the file's holder", .{ ino, what });
        return error.Transient;
    }

    // ---- the block-map tree: the crossing, the window merge, the sweep ----

    /// THE CROSSING (charter §5.6; block_map_tree.md §2) under the held
    /// layout lock: (1) the ratchet — `BLOCK_MAP_TREE` stamped and barriered
    /// before any record; (2) the A1 residue diff of the tree against the
    /// desired map (`merged`, the window folded in); (3) the ops in chunks
    /// of `kvmap_chunk` per tx; (4) the FLIP as the LAST tx: the head Put
    /// through the typed arm (the moved entries keep their references —
    /// the named-ref diff would release every block of the file) plus the
    /// window's own `±ref`. Every mid-train crash leaves records staged and
    /// the head not flipped: invisible after remount, reconciled by the
    /// next crossing's diff. The crossing counts, logs ONE INFO line.
    fn crossing(self: *DataPlane, req: seams.MergeRequest, prev: layout.Layout, coll_seq: u8, merged: []const layout.MapEntryWire, out: *seams.MergeOutcome) seams.MergeError!seams.MergeOutcome {
        const gpa = self.gpa;
        const ino = req.ino;
        self.backend.stampBlockMapTree() catch |e| {
            log.err("ino {d}: the BLOCK_MAP_TREE stamp failed ({s}) — the crossing is refused; the write keeps its custody and retries", .{ ino, @errorName(e) });
            return error.Transient;
        };
        var plan = kvmap.planCrossing(self.mapStore(), gpa, ino, merged, self.chunk) catch |e| return mapKvErr(e);
        defer plan.deinit(gpa);
        if (plan.preexisting != 0) kvmap.stats.map_migrate_resumed.inc();
        const size = @max(prev.size, req.min_size);
        const head = layout.Layout.stripedKvmap(size, null, kvmap.FIRST_MAP_GEN);
        // The kill seam separates the flip from the records so the pre-flip
        // state is reachable on a small map too.
        const flip_alone = self.crossing_kill_after != 0;
        // The window's displaced bindings (released at the flip) are live
        // owners until the caller's frees (decision 0003 §AH.1).
        const holds = self.releaseHolds();
        holds.hold(out.slice());
        errdefer holds.drop(out.slice());
        var pos: usize = 0;
        var chunks: u32 = 0;
        var flipped = false;
        while (!flipped) {
            const n = @min(self.kvmap_chunk, plan.ops.len - pos);
            const last_records = pos + n == plan.ops.len;
            const flip_here = last_records and (!flip_alone or n == 0);
            const t = self.backend.begin(@src()) catch return error.OutOfMemory;
            for (plan.ops[pos .. pos + n]) |op| kvmap.stageOp(t, ino, op) catch |e| {
                self.backend.abandon(t);
                return mapKvErr(e);
            };
            if (flip_here) {
                layout.publishLayout(t, .{ .ino = ino, .name_key = self.name_key, .coll_seq = coll_seq, .prev = prev, .layout = head, .proofs = req.proofs, .arm = .{ .flip_to_kvmap = .{ .moved = prev.entries.len } } }, self.refsSeam(), self.geo) catch |e| {
                    self.backend.abandon(t);
                    stats.layout_map_merge_failures.inc();
                    log.err("ino {d}: the block-map-tree flip failed to stage ({s})", .{ ino, @errorName(e) });
                    return switch (e) {
                        error.OutOfMemory => error.OutOfMemory,
                        else => error.Transient,
                    };
                };
                self.stageWindowRefs(t, ino, prev, req.inserts) catch {
                    self.backend.abandon(t);
                    return error.OutOfMemory;
                };
            }
            self.backend.commit(t) catch |e| {
                stats.layout_map_merge_failures.inc();
                log.err("ino {d}: a block-map-tree crossing tx failed to commit ({s}) — the records so far are invisible residue the next crossing reconciles", .{ ino, @errorName(e) });
                return switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    error.VolumeFailed, error.VolumeClosed => error.WriterGuardFenced,
                    else => error.Transient,
                };
            };
            pos += n;
            kvmap.stats.map_migrate_records.add(n);
            if (flip_here) {
                flipped = true;
            } else {
                chunks += 1;
                if (flip_alone and (chunks == self.crossing_kill_after or last_records)) {
                    log.err("TEST SEAM SQUEEZEFS_TEST_KVMAP_CROSSING_KILL_AFTER: ino {d}: {d} record chunk(s) committed, head NOT flipped — SIGKILL", .{ ino, chunks });
                    _ = linux.kill(linux.getpid(), .KILL);
                }
            }
        }
        kvmap.stats.kvmap_crossings.inc();
        stats.layout_map_merges.inc();
        stats.layout_map_merge_blocks.add(req.inserts.len);
        self.windows.invalidate(ino);
        log.info("ino {d}: block-map-tree crossing — {d} inline entries → {d} records ({d} ops over {d} tx), {d} preexisting record(s) reconciled; head flipped at size {d}, gen {d}", .{ ino, merged.len, plan.records, plan.ops.len, chunks + 1, plan.preexisting, size, kvmap.FIRST_MAP_GEN });
        out.holds = holds;
        return out.*;
    }

    /// The publishing window's own reference moves at the flip: `+ref` for
    /// an insert the inline map did not name at that block, `−ref` for the
    /// binding it displaced there.
    fn stageWindowRefs(self: *DataPlane, t: *tx_mod.KvTx, ino: u64, prev: layout.Layout, inserts: []const seams.MapInsert) Allocator.Error!void {
        for (inserts) |ins| {
            const old = prev.lookup(self.chunk, ins.block);
            if (old) |o| {
                if (o.sameRef(ins.binding, self.chunk)) continue;
                try block_refs.stageOps(t, &.{block_refs.released(.{ .vol_tag = o.vol_tag, .block_idx = o.blockIdx(self.chunk), .owner_ino = ino, .block_index = ins.block })});
            }
            try block_refs.stageOps(t, &.{block_refs.taken(.{ .vol_tag = ins.binding.vol_tag, .block_idx = ins.binding.blockIdx(self.chunk), .owner_ino = ino, .block_index = ins.block })});
        }
    }

    /// A merge on a tree-mapped head ships ONLY its window (block_map_tree.md
    /// §3.1): every insert's DMA proof checked, the inserts sorted, the
    /// extend barrier when the size floor passes a live sweep cursor, then
    /// ONE tx = `kvmap.stageWindow` (the records + the window's `±ref`) and
    /// the head Put only when size or cursor moved. Tree reads are O(window).
    fn kvmapMerge(self: *DataPlane, req: seams.MergeRequest, prev: layout.Layout, coll_seq: u8) seams.MergeError!seams.MergeOutcome {
        const gpa = self.gpa;
        const ino = req.ino;
        for (req.inserts) |ins| {
            var covered = false;
            for (req.proofs) |p| {
                if (p.covers(ins.binding, self.chunk)) covered = true;
            }
            if (!covered) {
                stats.layout_map_merge_failures.inc();
                log.err("ino {d} block {d}: a block-map publish without a completed DMA behind its binding — refused", .{ ino, ins.block });
                return error.Transient;
            }
        }
        const inserts = try gpa.dupe(seams.MapInsert, req.inserts);
        defer gpa.free(inserts);
        const n_unique = sortInsertsLastWins(inserts);
        const window = inserts[0..n_unique];
        const head = prev.kvmapHead().?;
        const size = @max(prev.size, req.min_size);
        const floor_blocks = self.blocksOf(size);
        var cursor: ?u32 = head.sweep;
        // The barrier's freed bindings are held from before each sweep
        // page's commit (decision 0003 §AH.1) until the deferred frees.
        var barrier_frees: std.ArrayList(Mapping) = .empty;
        defer barrier_frees.deinit(gpa);
        errdefer self.releaseHolds().drop(barrier_frees.items);
        if (cursor) |k| if (floor_blocks > k) {
            cursor = try self.barrierSweep(ino, k, floor_blocks, &barrier_frees);
        };
        if (cursor) |k| for (window) |ins| if (ins.block >= k) {
            stats.layout_map_merge_failures.inc();
            log.err("ino {d} block {d}: a binding at or above the live sweep cursor {d} — a cursor-invariant break upstream; refused", .{ ino, ins.block, k });
            return error.Transient;
        };
        const t = self.backend.begin(@src()) catch return error.OutOfMemory;
        var out: seams.MergeOutcome = .{};
        kvmap.stageWindow(self.mapStore(), gpa, t, ino, window, self.chunk, &out) catch |e| {
            self.backend.abandon(t);
            stats.layout_map_merge_failures.inc();
            log.err("ino {d}: the window put of {d} binding(s) failed ({s}) — the upload keeps its custody and retries", .{ ino, window.len, @errorName(e) });
            return mapKvErr(e);
        };
        if (size != prev.size or !std.meta.eql(cursor, head.sweep)) {
            layout.publishLayout(t, .{ .ino = ino, .name_key = self.name_key, .coll_seq = coll_seq, .prev = prev, .layout = layout.Layout.stripedKvmap(size, cursor, head.gen orelse kvmap.FIRST_MAP_GEN), .proofs = &.{} }, self.refsSeam(), self.geo) catch |e| {
                self.backend.abandon(t);
                stats.layout_map_merge_failures.inc();
                return switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    else => error.Transient,
                };
            };
        }
        // The window's displaced bindings are live owners until the
        // caller's frees (decision 0003 §AH.1).
        const holds = self.releaseHolds();
        holds.hold(out.slice());
        self.backend.commit(t) catch |e| {
            holds.drop(out.slice());
            stats.layout_map_merge_failures.inc();
            log.err("ino {d}: the block-map commit of {d} binding(s) failed ({s})", .{ ino, window.len, @errorName(e) });
            return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.VolumeFailed, error.VolumeClosed => error.WriterGuardFenced,
                else => error.Transient,
            };
        };
        out.holds = holds;
        stats.layout_map_merges.inc();
        stats.layout_map_merge_blocks.add(window.len);
        kvmap.stats.kvmap_window_saves.inc();
        self.windows.invalidate(ino);
        // The barrier's frees: the caller holds its (3.5) stripe across this
        // merge, so they run on the meta pool after it returns (RES-1).
        if (barrier_frees.items.len != 0) {
            const owned = barrier_frees.toOwnedSlice(gpa) catch {
                self.releaseHolds().drop(barrier_frees.items);
                return out;
            };
            self.deferFrees(owned);
        }
        return out;
    }

    /// Sort inserts by block; among duplicates of one block the LATER one
    /// in the caller's order wins (the epoch feeds its shadows oldest
    /// first). Returns the unique length.
    fn sortInsertsLastWins(inserts: []seams.MapInsert) usize {
        // Insertion sort: stable, and the windows are ≤ 64 entries.
        var i: usize = 1;
        while (i < inserts.len) : (i += 1) {
            const v = inserts[i];
            var j = i;
            while (j > 0 and inserts[j - 1].block > v.block) : (j -= 1) inserts[j] = inserts[j - 1];
            inserts[j] = v;
        }
        var n: usize = 0;
        for (inserts, 0..) |ins, k| {
            if (k + 1 < inserts.len and inserts[k + 1].block == ins.block) continue;
            inserts[n] = ins;
            n += 1;
        }
        return n;
    }

    /// THE EXTEND BARRIER (block_map_tree.md §4.3): a size floor past the
    /// live cursor `k` would make the residue in `[k, floor)` servable the
    /// instant it commits, so it is swept first — chunked txs, each with the
    /// cursor advance (a crash re-runs record-true). Returns the cursor the
    /// merge's head carries: `floor` (or null when the whole residue went).
    fn barrierSweep(self: *DataPlane, ino: u64, k: u32, floor: u64, frees: *std.ArrayList(Mapping)) seams.MergeError!?u32 {
        const gpa = self.gpa;
        const end: u32 = @intCast(@min(floor, std.math.maxInt(u32)));
        var start = k;
        while (true) {
            var page = kvmap.planSweepPage(self.mapStore(), gpa, ino, start, end, self.kvmap_chunk, self.chunk) catch |e| return mapKvErr(e);
            defer page.deinit(gpa);
            const next: ?u32 = page.next_cursor;
            if (page.isEmpty() and next == null) break;
            const t = self.backend.begin(@src()) catch return error.OutOfMemory;
            kvmap.stageSweepPage(t, ino, &page, self.chunk) catch |e| {
                self.backend.abandon(t);
                return mapKvErr(e);
            };
            const rec = (self.layoutRecord(ino) catch return error.Transient) orelse {
                self.backend.abandon(t);
                return error.NotFound;
            };
            defer gpa.free(rec.value);
            const prev = layout.XattrLayout.decode(rec.value, self.geo) catch {
                self.backend.abandon(t);
                return error.Transient;
            };
            const cursor_after: u32 = next orelse end;
            layout.publishLayout(t, .{ .ino = ino, .name_key = self.name_key, .coll_seq = rec.coll_seq, .prev = prev, .layout = layout.Layout.stripedKvmap(prev.size, cursor_after, prev.kvmapHead().?.gen orelse kvmap.FIRST_MAP_GEN), .proofs = &.{} }, self.refsSeam(), self.geo) catch {
                self.backend.abandon(t);
                return error.Transient;
            };
            // The page's freed bindings join the caller's list (held) before
            // the commit releases them (decision 0003 §AH.1).
            const before = frees.items.len;
            frees.ensureUnusedCapacity(gpa, page.freed.items.len) catch {
                self.backend.abandon(t);
                return error.OutOfMemory;
            };
            for (page.freed.items) |f| frees.appendAssumeCapacity(f.binding);
            self.releaseHolds().hold(frees.items[before..]);
            self.backend.commit(t) catch |e| {
                self.releaseHolds().drop(frees.items[before..]);
                frees.shrinkRetainingCapacity(before);
                return switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    error.VolumeFailed, error.VolumeClosed => error.WriterGuardFenced,
                    else => error.Transient,
                };
            };
            kvmap.stats.map_sweep_chunks.inc();
            self.windows.invalidate(ino);
            if (next == null) break;
            start = next.?;
        }
        return end;
    }

    /// Held releases (decision 0003 §AH.1) freed on the meta pool, their
    /// holds dropped after.
    fn deferFrees(self: *DataPlane, bindings: []Mapping) void {
        self.tasks.spawn(self.io, deferredFreeTask, .{ self, bindings }) catch {
            // Never leaked: the frees run here (the one RES-1 exception the
            // executor's refusal forces).
            self.freeReleased(self.io, bindings);
            self.gpa.free(bindings);
        };
    }

    fn deferredFreeTask(self: *DataPlane, bindings: []Mapping) Io.Cancelable!void {
        self.freeReleased(self.io, bindings);
        self.gpa.free(bindings);
    }

    // ---- the sweep (block_map_tree.md §4): the data face + the job ----

    /// The SIZE-FLIP-FIRST shrink of a tree-mapped file, staged into the
    /// caller's tx: ONE head Put `{size, sweep = min(existing, K)}`, K =
    /// ceil(size / block) — nothing else (a 1 PiB truncate is one record);
    /// the records at or above the cursor are residue the sweep drains
    /// (`drainKvmapSweep` after the commit). The caller HOLDS the ino's
    /// layout lock (3.7) from before it read `prev` until its commit landed
    /// (`lockLayout`; module doc) — the head's one authority; a Put staged
    /// outside it is counted. Returns the cursor planted.
    pub fn stageKvmapShrink(self: *DataPlane, t: *tx_mod.KvTx, ino: u64, prev: layout.Layout, coll_seq: u8, new_size: u64) Error!u32 {
        if (!lock_order.holds(.layout)) stats.kvmap_head_put_unlocked.fire("a tree-mapped head Put staged outside the layout lock");
        const head = prev.kvmapHead() orelse return error.InvalidArgument;
        const k: u32 = @intCast(@min(self.blocksOf(new_size), std.math.maxInt(u32) - 1));
        const cursor = if (head.sweep) |c| @min(c, k) else k;
        layout.publishLayout(t, .{ .ino = ino, .name_key = self.name_key, .coll_seq = coll_seq, .prev = prev, .layout = layout.Layout.stripedKvmap(new_size, cursor, head.gen orelse kvmap.FIRST_MAP_GEN), .proofs = &.{} }, self.refsSeam(), self.geo) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => error.InputOutput,
        };
        kvmap.stats.map_sweep_jobs.inc();
        return cursor;
    }

    /// The CORPSE mark of an unlinked tree-mapped file: the head Put `{size
    /// 0, sweep 0}` into the caller's tx; the inode record stays (nlink 0,
    /// unreachable) until the sweep's terminal chunk destroys it with its
    /// xattrs.
    pub fn stageKvmapCorpse(self: *DataPlane, t: *tx_mod.KvTx, ino: u64, prev: layout.Layout, coll_seq: u8) Error!void {
        _ = try self.stageKvmapShrink(t, ino, prev, coll_seq, 0);
    }

    /// After a shrink's or a corpse mark's tx landed: drain the residue —
    /// synchronously (the caller's op) when at most `chunk × 64` blocks
    /// were removed, on a meta lane above it.
    pub fn drainKvmapSweep(self: *DataPlane, ino: u64, removed_blocks: u64) void {
        if (removed_blocks <= kvmap.sweepThresholdBlocks(self.kvmap_chunk)) {
            self.sweepJob(ino, false);
        } else {
            self.spawnSweep(ino);
        }
    }

    fn spawnSweep(self: *DataPlane, ino: u64) void {
        const lanes = self.sqz.metaLanes();
        const lane = if (lanes.len == 0) self.sqz.homeLane(null, null) else lanes[@intCast(self.meta_rr.fetchAdd(1, .monotonic) % lanes.len)];
        self.sqz.spawnGroupOn(lane, &self.sweep_group, sweepFiber, .{ self, ino }) catch |e| {
            stats.kvmap_sweep_spawn_refusals.inc();
            log.err("ino {d}: the block-map sweep could not be spawned ({s}) — it resumes from its durable cursor at the next mount", .{ ino, @errorName(e) });
        };
    }

    fn sweepFiber(self: *DataPlane, ino: u64) void {
        self.sweepJob(ino, true);
    }

    fn registerSweep(self: *DataPlane, ino: u64) bool {
        self.sweeps_lock.lock();
        defer self.sweeps_lock.unlock();
        const gop = self.sweeps.getOrPut(self.gpa, ino) catch return false;
        return !gop.found_existing;
    }

    fn unregisterSweep(self: *DataPlane, ino: u64) void {
        self.sweeps_lock.lock();
        defer self.sweeps_lock.unlock();
        _ = self.sweeps.remove(ino);
    }

    /// The cursor-resumed sweep: one chunk tx per pass until the terminal
    /// chunk clears the cursor (or destroys the corpse). Deduped per ino
    /// (a second submission is safe — every chunk re-reads the cursor —
    /// but pointless). `throttled` = the manners law between chunks:
    /// foreground device-byte movement since the last chunk yields one
    /// tick; idle runs full width. The stop latch ends it at a chunk
    /// boundary; the next mount adopts the cursor.
    fn sweepJob(self: *DataPlane, ino: u64, throttled: bool) void {
        if (!self.registerSweep(ino)) return;
        defer self.unregisterSweep(ino);
        var fg = foregroundBytes(null);
        while (!self.sweeps_stop.load(.acquire)) {
            const outcome = self.sweepChunk(ino) catch |e| {
                log.err("ino {d}: a block-map sweep chunk failed ({s}) — the sweep resumes from its durable cursor at the next kick or mount", .{ ino, @errorName(e) });
                return;
            };
            if (outcome == .done) return;
            if (throttled) {
                const now_fg = foregroundBytes(null);
                if (now_fg != fg) {
                    fg = now_fg;
                    self.io.sleep(.{ .nanoseconds = reclaim.MANNERS_TICK_MS * std.time.ns_per_ms }, .awake) catch {};
                }
            }
        }
    }

    const ChunkOutcome = enum { more, done };

    /// ONE sweep chunk under the layout lock: re-read the head (the plan IS
    /// the durable cursor), `start = max(cursor, ceil(head.size / block))`
    /// — the HEAD's size, the record the cursor lives in and every merge
    /// raises, so a write that grew the file back never has its minted
    /// records deleted and a corpse (head size 0) sweeps whole — one page
    /// by the index budget, ONE tx = the Deletes / shortened Puts + their
    /// `−ref` + the cursor advance (the terminal chunk clears the cursor,
    /// or destroys the corpse: inode + xattrs). The head Put is CONDITIONAL
    /// on the re-read: `prev` is the record as it stands at commit time
    /// because the lock spans read → stage → commit, and every other head
    /// writer (the merge, kv_fs's shrink and corpse mark) commits under the
    /// same lock — so the fold is last-writer-wins over SERIAL writers and a
    /// truncate racing a chunk never loses its size nor the chunk its
    /// cursor (`plane_locks_model.HeadFoldModel`; 0002 row 210). The freed
    /// blocks go to the reclaimer AFTER the lock drops (RES-1).
    fn sweepChunk(self: *DataPlane, ino: u64) Error!ChunkOutcome {
        const io = self.io;
        const gpa = self.gpa;
        var frees: std.ArrayList(Mapping) = .empty;
        defer frees.deinit(gpa);
        var outcome: ChunkOutcome = .done;
        {
            const lock = self.lockLayout(io, ino);
            defer lock.release(io);
            const rec = (try self.layoutRecord(ino)) orelse return .done;
            defer gpa.free(rec.value);
            const prev = layout.XattrLayout.decode(rec.value, self.geo) catch return error.InputOutput;
            const head = prev.kvmapHead() orelse return .done;
            const k = head.sweep orelse return .done;
            const inode = try self.inodeRecord(ino);
            const nlink: u32 = if (inode) |v| v.nlink else 0;
            const start: u32 = @intCast(@min(@max(@as(u64, k), self.blocksOf(prev.size)), std.math.maxInt(u32)));
            var page = kvmap.planSweepPage(self.mapStore(), gpa, ino, start, std.math.maxInt(u32), self.kvmap_chunk, self.chunk) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                else => error.InputOutput,
            };
            defer page.deinit(gpa);
            const t = self.backend.begin(@src()) catch return error.OutOfMemory;
            kvmap.stageSweepPage(t, ino, &page, self.chunk) catch |e| {
                self.backend.abandon(t);
                return switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    else => error.InputOutput,
                };
            };
            const gen = head.gen orelse kvmap.FIRST_MAP_GEN;
            if (page.next_cursor) |next| {
                if (next != k) self.publishHead(t, ino, rec.coll_seq, prev, layout.Layout.stripedKvmap(prev.size, next, gen)) catch |e| {
                    self.backend.abandon(t);
                    return e;
                };
                outcome = .more;
            } else if (nlink == 0 and inode != null) {
                self.stageDestroyCorpse(t, ino) catch |e| {
                    self.backend.abandon(t);
                    return e;
                };
            } else {
                self.publishHead(t, ino, rec.coll_seq, prev, layout.Layout.stripedKvmap(prev.size, null, gen)) catch |e| {
                    self.backend.abandon(t);
                    return e;
                };
            }
            // The page's freed bindings are live owners from before the
            // commit until after their frees (decision 0003 §AH.1).
            frees.ensureTotalCapacity(gpa, page.freed.items.len) catch {
                self.backend.abandon(t);
                return error.OutOfMemory;
            };
            for (page.freed.items) |f| frees.appendAssumeCapacity(f.binding);
            self.releaseHolds().hold(frees.items);
            self.backend.commit(t) catch |e| {
                self.releaseHolds().drop(frees.items);
                return KvBackend.coreError(e);
            };
            kvmap.stats.map_sweep_chunks.inc();
            if (outcome == .done and nlink == 0 and inode != null) kvmap.stats.kvmap_corpses_destroyed.inc();
            self.windows.invalidate(ino);
        }
        self.freeReleased(io, frees.items);
        return outcome;
    }

    fn publishHead(self: *DataPlane, t: *tx_mod.KvTx, ino: u64, coll_seq: u8, prev: layout.Layout, new: layout.Layout) Error!void {
        layout.publishLayout(t, .{ .ino = ino, .name_key = self.name_key, .coll_seq = coll_seq, .prev = prev, .layout = new, .proofs = &.{} }, self.refsSeam(), self.geo) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            else => error.InputOutput,
        };
    }

    /// The corpse's destroy: the inode record and every xattr of the ino
    /// (the head among them) in the caller's tx.
    fn stageDestroyCorpse(self: *DataPlane, t: *tx_mod.KvTx, ino: u64) Error!void {
        const gpa = self.gpa;
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(gpa, &kvs);
        const start = key.xattrKey(ino, 0, 0);
        const end = key.xattrKey(ino, key.HASH56_MAX, std.math.maxInt(u8));
        self.backend.range(gpa, key.TREE_XATTRS, &start, &end, 1024, &kvs) catch |e| return KvBackend.coreError(e);
        const xkeys = try gpa.alloc([]const u8, kvs.items.len);
        defer gpa.free(xkeys);
        for (kvs.items, 0..) |kv, i| xkeys[i] = kv.key;
        try inos.stageDestroy(t, ino, xkeys);
    }

    fn inodeRecord(self: *DataPlane, ino: u64) Error!?record.InodeValue {
        const bytes = (self.backend.lookup(self.gpa, key.TREE_INODES, &key.inodeKey(ino)) catch |e| return KvBackend.coreError(e)) orelse return null;
        defer self.gpa.free(bytes);
        return record.InodeValue.decode(bytes) catch return error.InputOutput;
    }

    /// Mount adoption: every tree-mapped head with a live cursor regenerates
    /// its job (the plan is the durable cursor; a corpse's job ends in its
    /// destroy).
    fn adoptSweeps(self: *DataPlane) void {
        const Adopt = struct {
            plane: *DataPlane,
            n: u64 = 0,
            fn visit(ctx: *@This(), ino: u64, l: layout.Layout) Error!void {
                const head = l.kvmapHead() orelse return;
                if (head.sweep == null) return;
                ctx.n += 1;
                kvmap.stats.map_sweep_resumed.inc();
                ctx.plane.spawnSweep(ino);
            }
        };
        var a: Adopt = .{ .plane = self };
        self.forEachLayout(*Adopt, &a, Adopt.visit) catch |e| {
            log.err("block-map sweep adoption at mount failed ({s}) — open sweeps resume at the next mount", .{@errorName(e)});
            return;
        };
        if (a.n != 0) log.info("block-map sweeps adopted at mount: {d} head(s) with a live cursor resumed", .{a.n});
    }

    /// fsck C11 (report-only, block_map_tree.md §5.3) at mount under the
    /// verify lever: (a) orphan records — an owner with no tree-mapped head
    /// (must stay 0); (b) a tree-mapped head with a size and no records; (c)
    /// a point on a foreign volume inside a run's span. Nothing is repaired.
    fn verifyBlockMapsAtMount(self: *DataPlane) void {
        const gpa = self.gpa;
        const Walk = struct {
            plane: *DataPlane,
            heads: std.AutoHashMapUnmanaged(u64, void) = .empty,
            empty_heads: u64 = 0,
            foreign: u64 = 0,
            fn visit(ctx: *@This(), ino: u64, l: layout.Layout) Error!void {
                if (!l.isKvmap()) return;
                try ctx.heads.put(ctx.plane.gpa, ino, {});
                const rep = kvmap.verifyBlockMap(ctx.plane.mapStore(), ctx.plane.gpa, ino, ctx.plane.chunk) catch return error.InputOutput;
                if (rep.records == 0 and l.size > 0) ctx.empty_heads += 1;
                ctx.foreign += rep.run_foreign_shadows;
            }
        };
        var w: Walk = .{ .plane = self };
        defer w.heads.deinit(gpa);
        self.forEachLayout(*Walk, &w, Walk.visit) catch |e| {
            log.err("C11 at mount: the head walk failed ({s})", .{@errorName(e)});
            return;
        };
        var orphans: u64 = 0;
        var owners: u64 = 0;
        var from: u64 = 0;
        while (kvmap.nextOwner(self.mapStore(), gpa, from) catch null) |owner| {
            owners += 1;
            if (!w.heads.contains(owner)) {
                const rep = kvmap.verifyBlockMap(self.mapStore(), gpa, owner, self.chunk) catch break;
                orphans += rep.records;
                log.err("C11: ino {d} owns {d} block-map record(s) but no tree-mapped head (crossing residue or a lost head) — report-only", .{ owner, rep.records });
            }
            if (owner == std.math.maxInt(u64)) break;
            from = owner + 1;
        }
        kvmap.stats.fsck_map_orphan_records.set(orphans);
        kvmap.stats.fsck_map_empty_heads.set(w.empty_heads);
        kvmap.stats.fsck_map_run_foreign_shadows.set(w.foreign);
        log.info("C11 at mount: {d} tree-mapped head(s), {d} owner(s) with records, {d} orphan record(s), {d} empty head(s), {d} run-foreign shadow(s)", .{ w.heads.count(), owners, orphans, w.empty_heads, w.foreign });
    }

    // ---- the read path's seams ----

    fn meta(self: *DataPlane) read_path.Meta {
        return .{ .ctx = self, .file = &metaFile, .binding = &metaBinding, .inline_data = &metaInlineData, .file_at = &metaFileAt };
    }

    fn metaFile(ctx: *anyopaque, ino: u64) ?read_path.FileInfo {
        return (metaFileAt(ctx, ino, std.math.maxInt(u64)) orelse return null).info;
    }

    /// `metaFile` + `metaBinding(block)` over ONE resolution (`resolveAt`:
    /// the read view, else one divert verdict and two store lookups — the
    /// warm ladder's per-read resolution). A block past `u32` asks the
    /// facts alone.
    fn metaFileAt(ctx: *anyopaque, ino: u64, block: u64) ?read_path.FileAt {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const b = std.math.cast(u32, block);
        const r = (self.resolveAt(ino, b) catch return null) orelse return null;
        const size = self.claimedSize(ino, r.size orelse return null);
        const p = r.point orelse return .{ .info = .{ .size = size, .kind = .inline_ }, .binding = null };
        const info: read_path.FileInfo = .{ .size = size, .kind = p.kind, .file_id = p.file_id, .durable_copy = p.durable_copy };
        if (p.kind != .striped or b == null) return .{ .info = info, .binding = null };
        return .{ .info = info, .binding = if (p.kvmap) |head| self.kvmapLookup(ino, head, b.?) else p.binding };
    }

    fn metaBinding(ctx: *anyopaque, ino: u64, block: u64) ?Mapping {
        if (block > std.math.maxInt(u32)) return null;
        return mapLookup(ctx, ino, @intCast(block));
    }

    /// The inline payload: off the view when it holds the ino, else the
    /// record's owned copy.
    fn metaInlineData(ctx: *anyopaque, ino: u64, offset: u64, out: []u8) usize {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        if (self.view.inlineData(ino, offset, out)) |n| return n;
        const rec = (self.layoutRecord(ino) catch return 0) orelse return 0;
        defer self.gpa.free(rec.value);
        const l = layout.XattrLayout.decode(rec.value, self.geo) catch return 0;
        if (l.kind != .inline_ or offset >= l.data.len) return 0;
        const n = @min(out.len, l.data.len - offset);
        @memcpy(out[0..n], l.data[@intCast(offset)..][0..n]);
        return n;
    }

    /// The volume binding the read path reads through: the zcrx funnel
    /// (the lane first, per call) over the device seam.
    fn readVolumes(self: *DataPlane) read_path.Volumes {
        self.zcrx_decorated = .{ .inner = .{ .ctx = self, .allocator_of = &volAllocatorOf, .read_range = &volReadRange, .device_fd = &volDeviceFd }, .funnel = &self.zcrx_funnel, .ordinal_of = &volOrdinalOf };
        return self.zcrx_decorated.volumes();
    }

    fn volOrdinalOf(ctx: *anyopaque, vol_tag: u64) ?u16 {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const v = self.vols.byTag(vol_tag) orelse return null;
        return @intCast(v.ordinal);
    }

    fn volAllocatorOf(ctx: *anyopaque, vol_tag: u64) ?*block_alloc.BlockAllocator {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const v = self.vols.byTag(vol_tag) orelse return null;
        return v.alloc;
    }

    /// The device DMA leg of the read seam (`read_path.Volumes.read_range`);
    /// the zcrx lane's gather, when armed, decides per call ahead of this.
    fn volReadRange(ctx: *anyopaque, vol_tag: u64, device_off: u64, dest: []u8, _: read_path.DestShape) read_path.DeviceError!read_path.Served {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        try self.deviceRead(vol_tag, device_off, dest, .read_path);
        return .dma;
    }

    fn volDeviceFd(ctx: *anyopaque, vol_tag: u64) ?linux.fd_t {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const dev = self.deviceOf(vol_tag) orelse return null;
        return dev.directFd();
    }

    /// ONE purge helper (charter §6.3) as the write side's seam.
    fn purgeSeam(self: *DataPlane) seams.PurgeSeam {
        return .{ .ctx = self, .purge_fn = &purgeBinding };
    }

    fn purgeBinding(ctx: ?*anyopaque, b: Mapping) void {
        const self: *DataPlane = @ptrCast(@alignCast(ctx.?));
        _ = purge.purgeBlockKey(self.rp.tiers, BlockKey.fromMapping(b));
    }

    fn reclaimPurge(ctx: ?*anyopaque, vol: u16, idx: u64) void {
        const self: *DataPlane = @ptrCast(@alignCast(ctx.?));
        const v = self.vols.byOrdinal(vol) orelse return;
        _ = purge.purgeBlockKey(self.rp.tiers, BlockKey.init(v.vol_tag, v.alloc.offsetOf(idx)));
    }

    fn fencePoll(ctx: ?*anyopaque) bool {
        const self: *DataPlane = @ptrCast(@alignCast(ctx.?));
        if (self.backend.isFailed()) return true;
        for (self.devs[0..self.vols.n()]) |d| if (d) |dev| if (dev.fenced()) return true;
        return false;
    }

    /// DEVICE-byte movement (never op counts): the manners law's signal.
    fn foregroundBytes(_: ?*anyopaque) u64 {
        return accumulate.stats.write_through_bytes.load() +% sole_owner_patch.stats.patch_write_bytes.load() +% device_overlay.stats.overlay_store_bytes.load() +% extent_overlay.stats.fold_inplace_write_bytes.load() +% accumulate.stats.drain_inplace_write_bytes.load() +% read_path.stats.ranged_read_bytes.load() +% read_path.stats.read_fill_dma_bytes.load() +% stats.own_block_dma_bytes.load();
    }

    // ---- the read seam over the write side ----

    fn writeSide(self: *DataPlane) read_path.WriteSide {
        return .{ .ctx = self, .overlay_read = &wsOverlayRead, .extent_runs = &wsExtentRuns, .staged_block = &wsStagedBlock, .staged_payload = &wsStagedPayload, .has_retained_extents = &wsHasRetainedExtents, .open_overlay = &wsOpenOverlay, .has_custody = &wsHasCustody };
    }

    /// Composed per block by `staged_block`; the whole-range face stays
    /// silent so a multi-block read composes block by block.
    fn wsOverlayRead(_: *anyopaque, _: u64, _: u64, _: u64, _: []u8) read_path.OverlayVerdict {
        return .none;
    }

    /// W2 runs are composed inside the write side's own `serve` (a
    /// partial answers its covered runs with the base named).
    fn wsExtentRuns(_: *anyopaque, _: u64, _: u64, _: u64, _: u64, _: []u8) read_path.ExtentCoverage {
        return .none;
    }

    /// The write side's block window (charter §6.3 precedence): the
    /// accumulation buffer's coverage-aware snapshot, the open overlay
    /// record, the rewrite epoch's shadow — a `partial` fills its gaps from
    /// the base binding the write side named (or zeros for a hole), a
    /// `redirect` reads the shadow key, a `settle` runs the serialized
    /// settle and falls to the map.
    fn wsStagedBlock(ctx: *anyopaque, ino: u64, block: u64, rel_s: u64, rel_e: u64, dest: []u8, try_only: bool) read_path.StagedBlock {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        if (block > std.math.maxInt(u32)) return .none;
        const b: u32 = @intCast(block);
        const view = self.wp.writeSideView();
        if (!view.hasCustody(ino, b)) return .none;
        const want: usize = @intCast(rel_e - rel_s);
        const out = dest[0..want];
        @memset(out, 0);
        // A try-only probe (the uring worker) serves RAM alone: a gap fill,
        // a shadow read or the settle is a device act — `busy`, the lane's.
        switch (view.serve(self.io, .{ .ino = ino, .block = b, .rel = @intCast(rel_s), .out = out, .try_only = try_only })) {
            .served => return .{ .served = want },
            .partial => |p| {
                const base = p.base orelse return .{ .served = want };
                // Every gap of the window from the base (one read per gap;
                // the page shape coalesces contiguous untouched pages).
                var gaps = p.covered.gaps(@intCast(rel_s), @intCast(rel_e));
                if (try_only) {
                    if (gaps.next() != null) return .busy;
                    return .{ .served = want };
                }
                while (gaps.next()) |g| self.fillGap(base, g.start, out[g.start - @as(u32, @intCast(rel_s)) .. g.end - @as(u32, @intCast(rel_s))]);
                return .{ .served = want };
            },
            .redirect => |shadow| {
                if (try_only) return .busy;
                self.fillGap(shadow, @intCast(rel_s), out);
                return .{ .served = want };
            },
            .settle => {
                if (try_only) return .busy;
                // The serialized settle: under (3), on the write arm's ladder.
                self.wp.overlay.drainBlock(self.io, &self.wp.locks, ino, b, false, self.wp.closeSeams());
                return .none;
            },
            .busy => return .busy,
            .none => return .none,
        }
    }

    /// `[rel, rel + out.len)` of the block `base` names, from the device
    /// (a failed read leaves zeros: the write side's custody still holds
    /// the newer bytes; a torn base is the next read's retry).
    fn fillGap(self: *DataPlane, base: Mapping, rel: u32, out: []u8) void {
        const w = base.readWindow(self.chunk, .{ .start = rel, .end = rel + out.len }) catch return;
        if (w.hi <= w.lo) return;
        self.deviceRead(w.vol_tag, w.device_off + w.lo, out[0 .. w.hi - w.lo], .compose) catch {};
    }

    /// This client's ring entry for a staged file.
    fn wsStagedPayload(ctx: *anyopaque, ino: u64, file_id: layout.FileId, offset: u64, len: u64, dest: []u8) read_path.StagedVerdict {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const st = &(self.stage orelse return .absent);
        const k = staging.StagingKey.init(file_id, ino);
        if (st.seg.get(k)) |lease| {
            defer lease.release();
            const want: usize = @intCast(@min(len, dest.len));
            // Whole against a racing in-place overwrite (the record's
            // seqlock); the bytes past the value are zeros.
            const n = lease.copyRange(self.io, dest[0..want], offset);
            @memset(dest[n..want], 0);
            return .{ .served = want };
        }
        if (st.pool.generationOf(file_id) != null) return .in_flight;
        return .absent;
    }

    /// The warm ladder's pre-screen, per read on the uring worker: the
    /// table's presence stripe answers without its lock (the locked
    /// `blocksOf` scan it ran was 9 % of a saturated worker).
    fn wsHasRetainedExtents(ctx: *anyopaque, ino: u64) bool {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        return self.wp.table.hasExtentOf(ino);
    }

    fn wsOpenOverlay(ctx: *anyopaque, ino: u64, block: u64) bool {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        if (block > std.math.maxInt(u32)) return false;
        return self.wp.overlay.hasCustody(ino, @intCast(block));
    }

    /// Any write-side custody of the block (the view's composed answer:
    /// accumulation/extent entry, overlay record, epoch shadow).
    fn wsHasCustody(ctx: *anyopaque, ino: u64, block: u64) bool {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        if (block > std.math.maxInt(u32)) return false;
        return self.wp.writeSideView().hasCustody(ino, @intCast(block));
    }

    // ---- the lane fibers: prefetch issuer, hold ceremony ----

    fn issuePrefetch(ctx: ?*anyopaque, ino: u64, block: u64, class: admission.FillClass) bool {
        const self: *DataPlane = @ptrCast(@alignCast(ctx.?));
        self.sqz.spawnGroupOn(self.sqz.homeLane(null, null), &self.lane_group, prefetchFiber, .{ self, ino, block, class }) catch {
            stats.prefetch_fiber_refusals.inc();
            return false;
        };
        stats.prefetch_fibers.inc();
        return true;
    }

    fn prefetchFiber(self: *DataPlane, ino: u64, block: u64, class: admission.FillClass) void {
        self.rp.warm(self.io, ino, block, class);
    }

    fn spawnCeremony(ctx: ?*anyopaque, k: BlockKey, class: admission.FillClass) void {
        const self: *DataPlane = @ptrCast(@alignCast(ctx.?));
        self.sqz.spawnGroupOn(self.sqz.homeLane(null, null), &self.lane_group, ceremonyFiber, .{ self, k, class }) catch return;
        stats.hold_ceremony_fibers.inc();
    }

    fn ceremonyFiber(self: *DataPlane, k: BlockKey, class: admission.FillClass) void {
        self.rp.holdCeremony(k, class);
    }

    // ---- the queue-thread gates (handler.zig's Binding calls these) ----

    /// The hold gate: a PURE predicate over the hint and the published
    /// shape — no lookup, no lock (`holdVerdict`); the direct-store face
    /// must be armed for either class.
    pub fn writeHoldEligible(self: *DataPlane, hint: fsv.HoldHint) bool {
        if (self.held_store == null) return declineHold();
        const snap = self.shape.peek(hint.ino) orelse return declineHold();
        const class = holdVerdict(hint, snap, .{
            .block_size = self.block_size,
            .patch_max = self.wp.cfg.patch_max,
            .prev_end = self.wp.last_write_end.peek(hint.ino),
            .overlay_hold = self.wp.cfg.overlay.hold_slot and self.wp.overlay.shapeEligible(hint.len, self.wp.any_passthrough),
        }) orelse return declineHold();
        stats.hold_gate_admits.inc();
        if (class == .overlay) stats.hold_gate_overlay_admits.inc();
        return true;
    }

    fn declineHold() bool {
        stats.hold_gate_declines.inc();
        return false;
    }

    /// What the gate knows besides the hint and the shape snapshot.
    const HoldFacts = struct {
        block_size: u64,
        patch_max: u64,
        /// The ino's previous write end (`last_write_end`), 0 = none.
        prev_end: u64,
        /// The overlay arm is on this set and its hold lever armed.
        overlay_hold: bool,
    };

    /// The gate's verdict (the tie test restates it): a striped file on a
    /// passthrough set, the write within one block, then by class
    /// (`zc.holdClass`: PATCH_MAX = block/8 is W1's ceiling and the
    /// overlay's length floor). The W1 class holds only what the sole-owner
    /// patch can take — non-extending, not stream-adjacent — because a
    /// held W1-ineligible shape pays hold + late extraction (0.45× on
    /// rand-4k at fabric RTT). The overlay class (charter §6.2 item 1) holds
    /// whenever the arm is on: extending and stream-adjacent writes ARE the
    /// streaming write it exists for (a fresh unpublished destination; the
    /// overwrite shape feeds the rewrite epoch), and both payload classes
    /// hold — the port lands an O_DIRECT slot synchronously by
    /// `WRITE_FIXED(device ← slot)` before the ack, never a late
    /// materialize (0002 row 544). What the arm declines under its guard
    /// (RAM custody on the block, an epoch's save in flight, a full
    /// registry, no supply) costs that write one lazy extraction, counted.
    fn holdVerdict(hint: fsv.HoldHint, snap: shape_table.Snap, f: HoldFacts) ?zc.HoldClass {
        if (hint.len == 0 or !snap.striped_passthrough) return null;
        const end = hint.offset + hint.len;
        if (hint.offset / f.block_size != (end - 1) / f.block_size) return null;
        return switch (zc.holdClass(hint.len, f.patch_max)) {
            .w1 => if (end > snap.size or f.prev_end == hint.offset) null else .w1,
            .overlay => if (f.overlay_hold) .overlay else null,
        };
    }

    /// The shared-mode admission probe (charter §6.2): a resident striped
    /// snapshot whose size the write stays within. `nodeid` is the
    /// kernel's; the table is keyed by the plane's effective ino.
    pub fn sharedProbe(self: *DataPlane, nodeid: u64, offset: u64, end: u64) ?op_guard.SharedProbe {
        _ = offset;
        const snap = self.shape.peek(shapeProbeKey(nodeid) orelse return null) orelse return null;
        if (!snap.striped_passthrough and self.passthrough) return null;
        return .{ .size_floor = snap.size, .meta_size = snap.size, .block_size = self.block_size, .range_fully_mapped = null, .narrow = end <= snap.size, .striped_passthrough = snap.striped_passthrough };
    }

    /// The shape table's key for a kernel nodeid (the lifecycle's probe):
    /// the plane publishes under the EFFECTIVE ino (`route_table.
    /// effectiveIno`, the key every layout record lives under), and the two
    /// spaces alias — the mint's rotor makes nodeid 4 slot 2's local 2 (key
    /// `(3 << 40) | 2`) while key 4 is slot 0's local 4 (nodeid 131074), so a
    /// raw nodeid probe read ANOTHER file's size (fstests generic/464:
    /// `write_shared_class_violation` ×193, every one an append at its own
    /// EOF classified Shared on a longer sibling). A nodeid outside the
    /// mint's space (a virtual control inode) has no shape.
    fn shapeProbeKey(nodeid: u64) ?u64 {
        if (!route_table.isRoutable(nodeid)) return null;
        return route_table.effectiveIno(nodeid);
    }

    pub fn probe(self: *DataPlane) op_guard.Probe {
        return .{ .ctx = self, .probe_fn = &probeFn };
    }

    // @anchor:ops_jobs_mover_faces — the ONE mover's probe + walk (rung 8).

    /// The mover's quiescence probe (`ops_jobs_lifecycle.md` §1.4): a
    /// referencer with LIVE write-side custody (a RAM buffer, an open
    /// device-overlay record, parked extents, a staged entry) is not
    /// quiescent — the task defers and re-plans.
    pub fn moverQuiescent(self: *DataPlane, ino: u64, block_index: u32) bool {
        // A write-leased inode is a client's to write in place: the mover's
        // copy would go stale under it (decision 0003 §AY).
        return !self.wp.writeSideView().hasCustody(ino, block_index) and !lease_epoch.inoWriteLeased(ino);
    }

    /// D3's seam (`defrag --fold`): the inos holding parked W2 extents, and
    /// the kick through the write side's own fold (the block lock (3) is
    /// the only lock the kick takes — the job's worker holds none).
    pub fn foldTargets(self: *DataPlane, gpa: Allocator) Allocator.Error![]u64 {
        return self.wp.foldTargets(gpa);
    }

    pub fn foldKick(self: *DataPlane, io: Io, ino: u64) u32 {
        return self.wp.foldParked(io, ino);
    }

    /// Every binding of every live file (the census walk the movers plan
    /// from): the inline map's and the packed tenant's off the record, a
    /// block-map-tree head's off its tree below the live sweep cursor (the
    /// indices at/above it are the sweep's, never a mover's; `rangeFrom`
    /// pages the tree in `kvmap_chunk`-record windows).
    pub fn forEachBinding(self: *DataPlane, comptime Ctx: type, ctx: Ctx, comptime visit: fn (Ctx, u64, u32, Mapping) Error!void) Error!void {
        const Walk = struct {
            plane: *DataPlane,
            inner: Ctx,
            fn layoutVisit(w: *@This(), ino: u64, l: layout.Layout) Error!void {
                const plane = w.plane;
                if (l.isKvmap()) {
                    const head = l.kvmapHead().?;
                    const ceiling: u64 = head.sweep orelse key.BLOCK_INDEX_MAP_BLOB;
                    var recs: std.ArrayList(kvmap.Rec) = .empty;
                    defer recs.deinit(plane.gpa);
                    var from: u64 = 0;
                    while (from < ceiling) {
                        recs.clearRetainingCapacity();
                        kvmap.rangeFrom(plane.mapStore(), plane.gpa, ino, @intCast(from), plane.kvmap_chunk, plane.chunk, &recs) catch |e| return switch (e) {
                            error.OutOfMemory => error.OutOfMemory,
                            else => error.InputOutput,
                        };
                        if (recs.items.len == 0) break;
                        for (recs.items) |r| {
                            var b: u32 = r.index;
                            while (b < r.end() and b < ceiling) : (b += 1) try visit(w.inner, ino, b, r.bindingAt(b, plane.chunk));
                        }
                        const last = recs.items[recs.items.len - 1];
                        if (last.end() <= from) break;
                        from = last.end();
                    }
                    return;
                }
                var it = l.named(plane.chunk);
                while (it.next()) |e| try visit(w.inner, ino, e.block, e.mapping);
            }
        };
        var w: Walk = .{ .plane = self, .inner = ctx };
        try self.forEachLayout(*Walk, &w, Walk.layoutVisit);
    }

    fn probeFn(ctx: ?*anyopaque, ino: u64, offset: u64, end: u64) ?op_guard.SharedProbe {
        const self: *DataPlane = @ptrCast(@alignCast(ctx.?));
        return self.sharedProbe(ino, offset, end);
    }

    // ---- the C8 oracle at mount ----

    /// The derived layout walk (every layout record's bindings) against the
    /// ledger, per volume; the verdict site counts the drift.
    fn verifyRefsAtMount(self: *DataPlane) void {
        const gpa = self.gpa;
        var owners: std.ArrayList(block_refs.OwnerBindings) = .empty;
        defer {
            for (owners.items) |o| gpa.free(o.bindings);
            owners.deinit(gpa);
        }
        self.derivedWalk(&owners) catch |e| {
            log.err("C8 at mount: the layout walk failed ({s})", .{@errorName(e)});
            return;
        };
        var rows: u64 = 0;
        for (self.vols.rows[0..self.vols.n()]) |r| {
            const v = r.?;
            var x: MountExempt = .{ .plane = self, .vol_tag = v.vol_tag };
            var report = block_refs.verifyRefs(gpa, self.refs_store, v.vol_tag, owners.items, x.exemptions()) catch |e| {
                log.err("C8 at mount: vol-{x:0>16}: {s}", .{ v.vol_tag, @errorName(e) });
                continue;
            };
            defer report.deinit(gpa);
            rows += report.rows.len;
            block_refs.confirm(gpa, self.refs_store, v.vol_tag, report);
            log.info("C8 at mount: vol-{x:0>16}: {d} drift row(s) over {d} owner(s)", .{ v.vol_tag, report.rows.len, owners.items.len });
            // C2 on the seeded allocator: an allocated block no reference
            // tracks (leaked) or a tracked block on the free list (lost).
            const c2 = v.alloc.fsckReconcileAccounting();
            log.info("C2 at mount: vol-{x:0>16}: {d} leaked block(s) freed, {d} lost block(s) evicted from the free list", .{ v.vol_tag, c2.frees_completed, c2.free_list_evictions });
        }
        stats.block_refs_drift_rows.set(rows);
    }

    /// C8 at mount's exemptions while it judges ONE volume.
    const MountExempt = struct {
        plane: *DataPlane,
        vol_tag: u64,

        fn exemptions(self: *MountExempt) block_refs.Exemptions {
            return .{ .ctx = self, .isOpenPack = &isOpenPack, .isInflight = &isInflight };
        }

        fn isOpenPack(ctx: ?*anyopaque, block_idx: u64) bool {
            const self: *MountExempt = @ptrCast(@alignCast(ctx.?));
            const p = &(self.plane.packer orelse return false);
            return p.isOpenBlock(self.vol_tag, block_idx * self.plane.chunk);
        }

        fn isInflight(ctx: ?*anyopaque, block_idx: u64) bool {
            const self: *MountExempt = @ptrCast(@alignCast(ctx.?));
            const v = self.plane.vols.byTag(self.vol_tag) orelse return false;
            return block_idx < v.alloc.capacityBlocks() and v.alloc.inflightContains(block_idx);
        }
    };

    /// Every layout record in the xattr tree → its owner's bindings: the
    /// inline map's entries, or — for a tree-mapped head — every record of
    /// the ino's block map expanded (the derived recovery walk owns a tree-
    /// mapped ino's blocks).
    fn derivedWalk(self: *DataPlane, owners: *std.ArrayList(block_refs.OwnerBindings)) Error!void {
        const Derive = struct {
            plane: *DataPlane,
            owners: *std.ArrayList(block_refs.OwnerBindings),
            fn visit(ctx: *@This(), ino: u64, l: layout.Layout) Error!void {
                const gpa = ctx.plane.gpa;
                var binds: std.ArrayList(block_refs.Binding) = .empty;
                errdefer binds.deinit(gpa);
                if (l.isKvmap()) {
                    kvmap.deriveBindings(ctx.plane.mapStore(), gpa, ino, ctx.plane.chunk, &binds) catch |e| return switch (e) {
                        error.OutOfMemory => error.OutOfMemory,
                        else => error.InputOutput,
                    };
                } else {
                    var it = l.named(ctx.plane.chunk);
                    while (it.next()) |e| try binds.append(gpa, .{ .block_index = e.block, .vol_tag = e.mapping.vol_tag, .block_idx = e.mapping.blockIdx(ctx.plane.chunk) });
                }
                try ctx.owners.append(gpa, .{ .ino = ino, .bindings = try binds.toOwnedSlice(gpa) });
            }
        };
        var d: Derive = .{ .plane = self, .owners = owners };
        try self.forEachLayout(*Derive, &d, Derive.visit);
    }

    /// Every layout record of the set, in key-ino order: the store merges
    /// every slot tree of every member for a range spanning slots (the
    /// native keyspace first, then the guest keyspaces slot by slot).
    fn forEachLayout(self: *DataPlane, comptime Ctx: type, ctx: Ctx, comptime visit: fn (Ctx, u64, layout.Layout) Error!void) Error!void {
        const gpa = self.gpa;
        var cursor = key.KeyBuf.fromSlice(&key.xattrKey(0, 0, 0));
        const end = key.xattrKey(std.math.maxInt(u64), key.HASH56_MAX, std.math.maxInt(u8));
        while (true) {
            var kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(gpa, &kvs);
            self.backend.range(gpa, key.TREE_XATTRS, cursor.slice(), &end, block_refs.SCAN_PAGE, &kvs) catch |e| return KvBackend.coreError(e);
            if (kvs.items.len == 0) return;
            // The slot tree interleaves every ino-major kind: only the
            // xattr records are decoded; the cursor resumes at the next
            // xattr key after the page's last key, whatever its kind.
            for (kvs.items) |kv| {
                if (kv.key.len != key.XATTR_KEY_LEN or kv.key[key.INO_MAJOR_KIND_OFF] != key.TREE_XATTRS) continue;
                const xv = record.XattrValue.decode(kv.value) catch continue;
                if (!std.mem.eql(u8, xv.name, layout.LAYOUT_XATTR_NAME)) continue;
                const l = layout.Layout.decode(xv.value, self.geo) catch continue;
                try visit(ctx, std.mem.readInt(u64, kv.key[0..8], .big), l);
            }
            if (kvs.items.len < block_refs.SCAN_PAGE) return;
            const last = kvs.items[kvs.items.len - 1].key;
            const last_ino = std.mem.readInt(u64, last[0..8], .big);
            const last_kind = if (last.len > key.INO_MAJOR_KIND_OFF) last[key.INO_MAJOR_KIND_OFF] else 0;
            if (last_kind < key.TREE_XATTRS) {
                cursor = key.KeyBuf.fromSlice(&key.xattrKey(last_ino, 0, 0));
            } else if (last_kind == key.TREE_XATTRS) {
                cursor = key.sameKindSuccessor(last) orelse return;
            } else {
                if (last_ino == std.math.maxInt(u64)) return;
                cursor = key.KeyBuf.fromSlice(&key.xattrKey(last_ino + 1, 0, 0));
            }
        }
    }

    // ---- stats ----

    /// Every data family under one object (the SIGUSR1 dump / `.stats`).
    pub fn writeFamiliesJson(self: *DataPlane, w: *Io.Writer) Io.Writer.Error!void {
        try w.writeAll("{\"plane\":");
        try writeStatsJson(w);
        try w.writeAll(",\"write\":");
        try write_admit.writeStatsJson(w);
        try w.writeAll(",\"read\":");
        try read_path.writeStatsJson(w);
        try w.writeAll(",\"warm_ladder\":");
        try warm_ladder.writeStatsJson(w);
        try w.writeAll(",\"read_view\":");
        try read_view.writeStatsJson(w);
        try w.writeAll(",\"layout\":");
        try layout.writeStatsJson(w);
        try w.writeAll(",\"kvmap\":");
        try kvmap.writeStatsJson(w);
        try w.writeAll(",\"staging\":");
        try staging.writeStatsJson(w);
        try w.writeAll(",\"staging_backing_phase_ns\":");
        try staging.writePhasesJson(w);
        try w.writeAll(",\"pack\":");
        try pack.writeStatsJson(w);
        try w.writeAll(",\"router\":");
        try router.writeStatsJson(w);
        try w.writeAll(",\"block_alloc\":");
        try block_alloc.writeStatsJson(w);
        try w.writeAll(",\"block_refs\":");
        try block_refs.writeStatsJson(w);
        try w.writeAll(",\"reclaim\":");
        try reclaim.writeStatsJson(w);
        try w.print(",\"publish_scratch\":{{\"layout_publish_scratch_slabs\":{d},\"layout_publish_scratch_slab_bytes\":{d},\"layout_publish_scratch_waits\":{d},\"layout_publish_scratch_overflow_bytes\":{d}}}", .{ self.publish_scratch.capacity(), self.publish_scratch.slabLen(), merge_scratch.stats.layout_publish_scratch_waits.load(), merge_scratch.stats.layout_publish_scratch_overflow_bytes.load() });
        try w.print(",\"transform\":{{\"compress_stored_raw\":{d},\"crypto_key_unwraps\":{d},\"crypto_key_unwrap_hits\":{d},\"crypto_unwrap_cache_full\":{d},\"transform_codec_pool_waits\":{d},\"transform_scratch_pool_waits\":{d},\"transform_codecs\":{d},\"transform_scratches\":{d}}}", .{ transform.stats.compress_stored_raw.load(), transform.stats.crypto_key_unwraps.load(), transform.stats.crypto_key_unwrap_hits.load(), transform.stats.crypto_unwrap_cache_full.load(), transform.stats.transform_codec_pool_waits.load(), transform.stats.transform_scratch_pool_waits.load(), if (self.xform) |*x| x.codecs.capacity() else 0, if (self.xform) |*x| x.scratches.capacity() else 0 });
        try w.writeAll(",\"nvme_dev\":");
        try nvme_dev.writeStatsJson(w);
        try w.writeAll(",\"purge\":");
        try purge.writeStatsJson(w);
        try w.writeAll(",\"hot_tier\":");
        try hot_tier.writeStatsJson(w);
        try w.writeAll(",\"nvme_read_cache\":");
        try nvme_read_cache.writeStatsJson(w);
        try w.writeAll(",\"prefetch\":");
        try prefetch.writeStatsJson(w);
        try w.writeAll(",\"placement\":");
        try self.rt.writePlacementJson(w);
        try w.writeAll(",\"volumes\":[");
        for (self.vols.rows[0..self.vols.n()], 0..) |r, i| {
            const v = r.?;
            if (i != 0) try w.writeAll(",");
            try w.print("{{\"vol_tag\":\"vol-{x:0>16}\",\"ordinal\":{d},\"used_blocks\":{d},\"free_blocks\":{d},\"virgin_blocks\":{d},\"capacity_blocks\":{d},\"fresh_supply_latched\":{}}}", .{ v.vol_tag, v.ordinal, v.alloc.usedBlocks() -| 1, v.alloc.freeBlocksCount(), v.alloc.virginBlocks(), v.alloc.capacityBlocks() - 1, v.alloc.freshSupplyLatched() });
        }
        try w.writeAll("]}");
    }
};

// ---------------------------------------------------------------------------
// Tests (portable arithmetic; the plane itself is exercised by the smoke).
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the hold gate's verdict by class (charter §6.2 items 1–2 on PATCH_MAX = block/8): the W1 class holds a non-extending, non-adjacent aligned write within one block and declines an extending or stream-adjacent one; the overlay class holds the streaming write — extending, stream-adjacent, both payload classes — iff the arm is on; a straddling write, an empty one and a non-striped file never hold" {
    const bs: u64 = 4 << 20;
    const facts: DataPlane.HoldFacts = .{ .block_size = bs, .patch_max = bs / 8, .prev_end = 0, .overlay_hold = true };
    const striped: shape_table.Snap = .{ .size = 3 * bs, .striped_passthrough = true };
    const hint = struct {
        fn at(offset: u64, len: u32, class: zc.PayloadClass) fsv.HoldHint {
            return .{ .ino = 7, .offset = offset, .len = len, .class = class };
        }
    };
    // W1: ≤ PATCH_MAX, inside the file, not adjacent to the previous write.
    try testing.expectEqual(@as(?zc.HoldClass, .w1), DataPlane.holdVerdict(hint.at(4096, 4096, .page_cache), striped, facts));
    try testing.expectEqual(@as(?zc.HoldClass, .w1), DataPlane.holdVerdict(hint.at(bs - (bs / 8), @intCast(bs / 8), .direct_gup), striped, facts));
    // W1 declines: extending; stream-adjacent (the previous write ended here).
    try testing.expectEqual(@as(?zc.HoldClass, null), DataPlane.holdVerdict(hint.at(3 * bs, 4096, .page_cache), striped, facts));
    var adjacent = facts;
    adjacent.prev_end = 8192;
    try testing.expectEqual(@as(?zc.HoldClass, null), DataPlane.holdVerdict(hint.at(8192, 4096, .page_cache), striped, adjacent));
    // The overlay class: a 1 MiB piece — extending the file, stream-adjacent, O_DIRECT — holds.
    const mib: u32 = 1 << 20;
    try testing.expectEqual(@as(?zc.HoldClass, .overlay), DataPlane.holdVerdict(hint.at(0, mib, .page_cache), striped, facts));
    try testing.expectEqual(@as(?zc.HoldClass, .overlay), DataPlane.holdVerdict(hint.at(3 * bs, mib, .direct_gup), striped, facts));
    adjacent.prev_end = 3 * bs + mib;
    try testing.expectEqual(@as(?zc.HoldClass, .overlay), DataPlane.holdVerdict(hint.at(3 * bs + mib, mib, .direct_gup), striped, adjacent));
    try testing.expectEqual(@as(?zc.HoldClass, .overlay), DataPlane.holdVerdict(hint.at(bs, @intCast(bs), .page_cache), striped, facts)); // a block-sized write
    // The overlay class declines with its arm off (the control arm, a transformed set, the lever at 0).
    var off = facts;
    off.overlay_hold = false;
    try testing.expectEqual(@as(?zc.HoldClass, null), DataPlane.holdVerdict(hint.at(0, mib, .page_cache), striped, off));
    // Never: a straddling write, an empty one, a file that is not striped on a passthrough set.
    try testing.expectEqual(@as(?zc.HoldClass, null), DataPlane.holdVerdict(hint.at(bs - mib + 4096, mib, .page_cache), striped, facts));
    try testing.expectEqual(@as(?zc.HoldClass, null), DataPlane.holdVerdict(hint.at(0, 0, .page_cache), striped, facts));
    try testing.expectEqual(@as(?zc.HoldClass, null), DataPlane.holdVerdict(hint.at(0, mib, .page_cache), .{ .size = 3 * bs, .striped_passthrough = false }, facts));
    // The patch-OFF control arm (PATCH_MAX 0): every aligned shape is the overlay's.
    var patch_off = facts;
    patch_off.patch_max = 0;
    try testing.expectEqual(@as(?zc.HoldClass, .overlay), DataPlane.holdVerdict(hint.at(4096, 4096, .page_cache), striped, patch_off));
}

// fstests generic/464 (`write_shared_class_violation` ×193 in one run, every
// one an append at the file's own EOF): the shape table is keyed by the
// plane's EFFECTIVE ino, the lifecycle probed it by the kernel NODEID, and
// the two spaces alias — the mint's rotor gives file #2 nodeid 4 (slot 2,
// local 2: key `(3 << 40) | 2`) and file #128 key 4 (slot 0, local 4: nodeid
// 131074), so file #2's writes were classified on file #128's size.
test "the shared probe keys by the plane's effective ino: a nodeid answers its own file's shape, never the file whose key merely equals that number" {
    var t = try ShapeTable.init(testing.allocator);
    defer t.deinit(testing.allocator);
    const nodeid_a: u64 = 4;
    const key_a = route_table.effectiveIno(nodeid_a);
    const nodeid_b = route_table.globalOfEffective(nodeid_a);
    try testing.expect(key_a != nodeid_a and nodeid_b != nodeid_a);
    t.publish(key_a, 1 << 20, true);
    t.publish(route_table.effectiveIno(nodeid_b), 6 << 20, true);
    try testing.expectEqual(@as(u64, 1 << 20), t.peek(DataPlane.shapeProbeKey(nodeid_a).?).?.size);
    try testing.expectEqual(@as(u64, 6 << 20), t.peek(DataPlane.shapeProbeKey(nodeid_b).?).?.size);
    // A nodeid outside the mint's space (a virtual control inode) probes nothing.
    try testing.expect(DataPlane.shapeProbeKey(0) == null);
    try testing.expect(DataPlane.shapeProbeKey(std.math.maxInt(u64) - 7) == null);
}

// leak67: the cache-path smoke row confirmed `[C2] … leaked` after `rm` on a
// two-volume set. The refill picked the volume again, so key A's open pack sat
// on B (the image transformed for A, landed on B), and fsck's open-pack face,
// looking the block up by its own volume, missed it.
test "a packed promotion's pack opens on the volume the promotion picked: the refill mints on the key's volume, never on a second router pick" {
    const gpa = testing.allocator;
    const chunk: u64 = 4 * derive.MiB;
    const pl = try gpa.create(DataPlane);
    defer gpa.destroy(pl);
    pl.io = testing.io;
    pl.chunk = chunk;
    pl.scope = pack.packScopeKey(0, 0);
    pl.vols = .{};
    // The valve is consulted only on a full volume; neither fills here.
    pl.reclaimer = undefined;
    pl.rt = try router.Router.init(gpa, chunk);
    defer pl.rt.deinit();
    const tags = [2]u64{ 0xA, 0xB };
    var allocs: [2]*block_alloc.BlockAllocator = undefined;
    for (tags, 0..) |t, i| {
        allocs[i] = try block_alloc.BlockAllocator.init(gpa, .{ .vol_tag = t, .vol = @intCast(i), .block_size = chunk, .capacity_bytes = 16 * chunk });
        pl.vols.add(.{ .vol_tag = t, .ordinal = @intCast(i), .alloc = allocs[i], .passthrough = true, .write_cache_volatile = false });
        try pl.rt.addVolume(.{ .id = .{ .tag = t }, .capacity_bytes = 16 * chunk });
    }
    defer for (allocs) |a| a.deinit();
    pl.packer = try pack.Packer.init(gpa, chunk, .{ .enabled = true, .max_slot = chunk / 2 }, pl.packAllocSeam(), pl.dmaSeam(), pl.packReadSeam(), router.MAX_VOLUMES);
    defer pl.packer.?.deinit();
    const packer = &pl.packer.?;
    // Two equal volumes, a round-robin pick: a second pick inside the refill
    // would name the other volume.
    for (0..2) |i| {
        const tag = (try pl.rt.pickVolume()).?;
        const k: pack.PackKey = .{ .vol_tag = tag, .scope = pl.scope };
        const tenant = (try packer.reserve(testing.io, k, 4096)).?;
        try testing.expectEqual(tag, tenant.pack.vol_tag);
        const open = packer.openPack(k).?;
        try testing.expectEqual(tag, open.vol_tag);
        // Tracked on the picked volume: the pack's pin + the tenant's.
        const a = pl.vols.byTag(tag).?.alloc;
        try testing.expectEqual(@as(?u32, 2), a.refcount(a.idxOf(open.base)));
        // The exemption faces match the block by its own volume and base (C8
        // at mount here; fsck's runner and the mover ask the same `isOpenBlock`).
        var on: DataPlane.MountExempt = .{ .plane = pl, .vol_tag = tag };
        const ex = on.exemptions();
        try testing.expect(ex.isOpenPack(ex.ctx, a.idxOf(open.base)));
        if (i == 0) {
            var off: DataPlane.MountExempt = .{ .plane = pl, .vol_tag = if (tag == tags[0]) tags[1] else tags[0] };
            const ex_off = off.exemptions();
            try testing.expect(!ex_off.isOpenPack(ex_off.ctx, a.idxOf(open.base)));
        }
    }
}

test "the staged point cache (the read view's validation law over one line): a fill is a hit under the same witness generation and move epoch and a miss under either moved; another ino in the slot is a miss; a fill in flight (seq odd) is a miss, not a torn read; a racing filler loses and the entry stays whole" {
    const c = try testing.allocator.create(DataPlane.StagedPointCache);
    defer testing.allocator.destroy(c);
    c.* = .{};
    const fid: layout.FileId = .{ 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16 };
    try testing.expect(c.lookup(7, 3, 1) == null);
    c.fill(7, 3, 1, 16384, fid);
    const hit = c.lookup(7, 3, 1).?;
    try testing.expectEqual(@as(u64, 16384), hit.size);
    try testing.expectEqualSlices(u8, &fid, &hit.file_id);
    try testing.expect(c.lookup(7, 4, 1) == null); // the witness moved
    try testing.expect(c.lookup(7, 3, 2) == null); // a slot move
    try testing.expect(c.lookup(7 + DataPlane.StagedPointCache.N, 3, 1) == null); // the slot's other ino
    // A fill in flight: the seq odd ⇒ a miss; a racing filler loses.
    const e = c.slot(7);
    const s = e.w[0].load(.acquire);
    e.w[0].store(s + 1, .release);
    try testing.expect(c.lookup(7, 3, 1) == null);
    c.fill(7, 9, 9, 1, fid);
    try testing.expectEqual(s + 1, e.w[0].load(.acquire));
    e.w[0].store(s + 2, .release);
    try testing.expectEqual(@as(u64, 16384), c.lookup(7, 3, 1).?.size);
}

test "stagingCapacity law: clamp(free/4, 64 MiB, budget/8) on the record grain" {
    const budget_bytes: u64 = 16 * derive.GiB;
    try testing.expectEqual(STAGING_FLOOR, DataPlane.stagingCapacityFor(1 << 20, budget_bytes));
    try testing.expectEqual(@as(u64, 2 * derive.GiB), DataPlane.stagingCapacityFor(1 << 40, budget_bytes));
    try testing.expectEqual(@as(u64, 1 << 30), DataPlane.stagingCapacityFor(4 * derive.GiB, budget_bytes));
    // A budget below 8 × the floor keeps the floor (never a non-ring).
    try testing.expectEqual(STAGING_FLOOR, DataPlane.stagingCapacityFor(1 << 40, 100 * derive.MiB));
}

test "foldInserts: the inserts fold ascending whatever order the epoch fed them; a replaced binding is displaced, a same-reference re-publish is not" {
    const gpa = testing.allocator;
    const chunk: u64 = 4 << 20;
    const old = [_]layout.MapEntryWire{
        (layout.MapEntry{ .block = 1, .mapping = Mapping.wholeBlock(0x10, 1 * chunk, 1) }).encode(),
        (layout.MapEntry{ .block = 4, .mapping = Mapping.wholeBlock(0x10, 4 * chunk, 1) }).encode(),
    };
    // Fed 9, 4 (same ref), 0, 1 (a new binding): the map must come out 0 1 4 9.
    const inserts = [_]seams.MapInsert{
        .{ .block = 9, .binding = Mapping.wholeBlock(0x10, 9 * chunk, 1) },
        .{ .block = 4, .binding = Mapping.wholeBlock(0x10, 4 * chunk, 1) },
        .{ .block = 0, .binding = Mapping.wholeBlock(0x10, 20 * chunk, 1) },
        .{ .block = 1, .binding = Mapping.wholeBlock(0x10, 21 * chunk, 1) },
    };
    var merged: std.ArrayList(layout.MapEntryWire) = .empty;
    defer merged.deinit(gpa);
    var out: seams.MergeOutcome = .{};
    try DataPlane.foldInserts(gpa, &old, &inserts, chunk, &merged, &out);
    try testing.expectEqual(@as(usize, 4), merged.items.len);
    for (merged.items, [_]u32{ 0, 1, 4, 9 }) |e, want| try testing.expectEqual(want, e.block());
    try testing.expectEqual(@as(u8, 1), out.n_displaced);
    try testing.expectEqual(@as(u64, 1 * chunk), out.displaced[0].offset);
    // The folded map is a legal striped layout (ascending is what publishLayout checks).
    _ = layout.Layout.stripedInline(10 * chunk, merged.items);
}

test "foldInserts displaces at most one binding per insert, so a merge of at most max_displaced inserts (the ONE save's buffer; every other publisher sends one) fills MergeOutcome exactly and never past it; a fold past the bound is loud (layout_map_merge_displaced_overflow), never a silent drop" {
    const gpa = testing.allocator;
    const chunk: u64 = 4 << 20;
    const max = seams.MergeOutcome.max_displaced;
    var old: [max + 1]layout.MapEntryWire = undefined;
    var inserts: [max + 1]seams.MapInsert = undefined;
    for (&old, &inserts, 0..) |*o, *ins, i| {
        const b: u32 = @intCast(i);
        o.* = (layout.MapEntry{ .block = b, .mapping = Mapping.wholeBlock(0x10, b * chunk, 1) }).encode();
        ins.* = .{ .block = b, .binding = Mapping.wholeBlock(0x10, (max + 1 + b) * chunk, 1) };
    }
    const site = "layout_map_merge_displaced_overflow";
    const fired0 = counters.tripwireCount(site);
    {
        var merged: std.ArrayList(layout.MapEntryWire) = .empty;
        defer merged.deinit(gpa);
        var out: seams.MergeOutcome = .{};
        try DataPlane.foldInserts(gpa, old[0..max], inserts[0..max], chunk, &merged, &out);
        try testing.expectEqual(@as(u8, max), out.n_displaced);
        try testing.expectEqual(fired0, counters.tripwireCount(site));
    }
    stats.layout_map_merge_displaced_overflow.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    var merged: std.ArrayList(layout.MapEntryWire) = .empty;
    defer merged.deinit(gpa);
    var out: seams.MergeOutcome = .{};
    try DataPlane.foldInserts(gpa, &old, &inserts, chunk, &merged, &out);
    try log.testing.expectAllFired();
    try testing.expectEqual(fired0 + 1, counters.tripwireCount(site));
}

test "stats JSON names every plane instrument" {
    var buf: [4096]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    const s = w.buffered();
    inline for (.{ "\"data_volumes\":", "\"layout_map_merges\":", "\"kvmap_sweep_spawn_refusals\":", "\"held_stores\":", "\"hold_gate_admits\":", "\"prefetch_fibers\":", "\"bindings_freed\":", "\"data_unresolved_volume\":", "\"kvmap_head_put_unlocked\":", "\"release_free_unheld\":", "\"layout_map_merge_displaced_overflow\":" }) |k| {
        try testing.expect(std.mem.indexOf(u8, s, k) != null);
    }
}
