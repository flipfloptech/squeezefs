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
//! spill of parked blocks into the ring; per-lane transform contexts (one
//! serialized `Transform`).

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
const sync_mutex = @import("../sync/mutex.zig");
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
const route_table = @import("../meta/routing/route_table.zig");
const op_guard = @import("../fuse/handler/op_guard.zig");
const fsv = @import("../fuse/transport/filesystem_vtable.zig");
const transport_slot = @import("../fuse/transport/slot.zig");
const layout = @import("layout.zig");
const kvmap = @import("kvmap.zig");
const staging = @import("staging.zig");
const pack = @import("pack.zig");
const router = @import("router.zig");
const block_alloc = @import("block_alloc.zig");
const block_refs = @import("block_refs.zig");
const reclaim = @import("reclaim.zig");
const enospc = @import("enospc.zig");
const transform = @import("transform.zig");
const set_config = @import("set_config.zig");
const data_superblock = @import("data_superblock.zig");
const seams = @import("write_seams.zig");
const write_admit = @import("write_admit.zig");
const write_side_view = @import("write_side_view.zig");
const device_overlay = @import("device_overlay.zig");
const patch_w1 = @import("patch_w1.zig");
const accumulate = @import("accumulate.zig");
const cov = @import("write_coverage.zig");
const fsync_mod = @import("fsync.zig");
const read_path = @import("read_path.zig");
const read_size_class = @import("read_size_class.zig");
const nvme_read_cache = @import("nvme_read_cache.zig");
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
/// The shape-snapshot table the W1 hold gate and the shared-mode probe
/// read on the queue thread: the op-guard width (one row per stripe).
pub const SHAPE_STRIPES: usize = op_guard.INODE_GUARD_STRIPES;

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
    /// The promotion dispatch at dismount / pressure / fsync.
    pub var promotions_run: counters.ShardedCounter = .{};
    pub var promotions_stale: counters.ShardedCounter = .{};
    pub var promotions_failed: counters.ShardedCounter = .{};
    /// Own-block images landed for a layout promotion (staged/inline → striped).
    pub var own_block_dmas: counters.ShardedCounter = .{};
    pub var own_block_dma_bytes: counters.ShardedCounter = .{};
    /// Direct stores of a held slot (`WRITE_FIXED(device ← slot)`) through the seam.
    pub var held_stores: counters.ShardedCounter = .{};
    pub var held_store_failures: counters.ShardedCounter = .{};
    /// W1 hold gate verdicts on the queue thread.
    pub var hold_gate_admits: counters.ShardedCounter = .{};
    pub var hold_gate_declines: counters.ShardedCounter = .{};
    /// Prefetch / ceremony fibers spawned on the home lanes.
    pub var prefetch_fibers: counters.ShardedCounter = .{};
    pub var prefetch_fiber_refusals: counters.ShardedCounter = .{};
    pub var hold_ceremony_fibers: counters.ShardedCounter = .{};
    /// Terminal frees handed to the reclaimer by the filesystem.
    pub var bindings_freed: counters.ShardedCounter = .{};
    /// The mount-time C8 oracle's verdict (0 = clean).
    pub var block_refs_drift_rows: counters.Gauge = .{};
    /// Device reads the plane bounced for an unaligned window.
    pub var device_read_bounces: counters.ShardedCounter = .{};
    /// A device read on a volume tag no mounted volume carries (must stay 0).
    pub var unresolved_volume: counters.Tripwire = .init("data_unresolved_volume");
};

pub fn writeStatsJson(w: *Io.Writer) Io.Writer.Error!void {
    try w.writeAll("{");
    inline for (.{ "data_volumes", "data_staging_on", "data_nvme_tier_on", "block_refs_drift_rows" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    inline for (.{ "layout_map_merges", "layout_map_merge_blocks", "layout_map_merge_failures", "kvmap_sweep_spawn_refusals", "promotions_run", "promotions_stale", "promotions_failed", "own_block_dmas", "own_block_dma_bytes", "held_stores", "held_store_failures", "hold_gate_admits", "hold_gate_declines", "prefetch_fibers", "prefetch_fiber_refusals", "hold_ceremony_fibers", "bindings_freed", "device_read_bounces" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    try w.print("\"data_unresolved_volume\":{d},\"kvmap_resolve_failures\":{d},\"kvmap_head_put_unlocked\":{d}}}", .{ counters.tripwireCount("data_unresolved_volume"), counters.tripwireCount("kvmap_resolve_failures"), counters.tripwireCount("kvmap_head_put_unlocked") });
}

// ---------------------------------------------------------------------------
// The shape table: what the queue thread may know about an inode.
// ---------------------------------------------------------------------------

/// One published snapshot per op-guard stripe: the inode's size and its
/// layout class, written by the filesystem at every layout publish and
/// size change, read lock-free (three relaxed loads) by the W1 hold gate
/// and the shared-mode probe. A torn read across the three words costs a
/// misjudged HOLD (performance) or a Shared candidate the admission
/// revalidates under the guard — never a wrong byte.
const ShapeRow = struct {
    ino: Atomic(u64) = .init(0),
    size: Atomic(u64) = .init(0),
    /// bit 0 = striped on a passthrough set.
    flags: Atomic(u64) = .init(0),
};

const SHAPE_STRIPED_PASSTHROUGH: u64 = 1;

const ShapeTable = struct {
    rows: []ShapeRow,

    fn init(gpa: Allocator) Allocator.Error!ShapeTable {
        const rows = try gpa.alloc(ShapeRow, SHAPE_STRIPES);
        @memset(rows, .{});
        return .{ .rows = rows };
    }

    fn deinit(self: *ShapeTable, gpa: Allocator) void {
        gpa.free(self.rows);
    }

    fn row(self: *ShapeTable, ino: u64) *ShapeRow {
        return &self.rows[@intCast(dlm.splitmix64(ino) & (SHAPE_STRIPES - 1))];
    }

    fn publish(self: *ShapeTable, ino: u64, size: u64, striped_passthrough: bool) void {
        const r = self.row(ino);
        // The ino word LAST (release): a reader that matches the ino sees
        // the size and flags published before it.
        r.size.store(size, .monotonic);
        r.flags.store(if (striped_passthrough) SHAPE_STRIPED_PASSTHROUGH else 0, .monotonic);
        r.ino.store(ino, .release);
    }

    fn forget(self: *ShapeTable, ino: u64) void {
        const r = self.row(ino);
        if (r.ino.load(.monotonic) == ino) r.ino.store(0, .release);
    }

    const Snap = struct { size: u64, striped_passthrough: bool };

    fn peek(self: *ShapeTable, ino: u64) ?Snap {
        const r = self.row(ino);
        if (r.ino.load(.acquire) != ino) return null;
        return .{ .size = r.size.load(.monotonic), .striped_passthrough = r.flags.load(.monotonic) & SHAPE_STRIPED_PASSTHROUGH != 0 };
    }
};

// ---------------------------------------------------------------------------
// The plane.
// ---------------------------------------------------------------------------

/// The held-slot store face the session installs once it is armed.
pub const HeldStoreFace = struct {
    ctx: *anyopaque,
    store: *const fn (ctx: *anyopaque, src: transport_slot.HeldSource, fd: linux.fd_t, off: u64, io: Io) transport_slot.ExtractError!void,
};

const Staging = struct {
    root_fd: linux.fd_t,
    mapped: staging.Ring.MappedSegment,
    seg: staging.Segment,
    pool: staging.Pool,
};

const Xform = struct {
    t: transform.Transform,
    crypto: ?*transform.CryptoState,
    /// One `Transform` (the zstd context is single-threaded): serialized.
    lock: sync_mutex.Mutex = .init,
    scratch: []align(layout.GRAIN) u8,
    read_scratch: []align(layout.GRAIN) u8,
};

pub const Space = struct { total_bytes: u64, free_bytes: u64 };

pub const Options = struct {
    env: *const std.process.Environ.Map,
    facts: derive.Facts,
    budget_bytes: u64,
    /// The volume key (an encrypted set); zeroized by the caller after.
    derived_key: ?transform.DerivedKey,
    /// The level-1 guard table the warm ladder try-locks.
    guards: *op_guard.Table,
};

pub const OpenError = error{ OutOfMemory, DeviceRefused, IdentityMismatch, Io, StagingRefused, TransformRefused, ReclaimRefused, InlineMaxAboveFormatBound };

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
    hash_seed: u64,
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
    /// The token divert's layout face (bound by the daemon once the wire
    /// plane is up; null = a flat mount: every slot local).
    foreign_layout: ?ForeignLayout = null,
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
            .hash_seed = be.hashSeed(),
            .passthrough = tcfg.isPassthrough(),
            .rt = undefined,
            .reclaimer = undefined,
            .reclaim_free = undefined,
            .wp = undefined,
            .rp = undefined,
            .ladder = undefined,
            .shape = undefined,
            .file_id_seq = .init(@bitCast(clock.realtimeCoarseNs())),
            .refs_store = block_refs.RefStore.from(KvBackend, be),
            // Rung 4: one meta volume, the native slot — scope 0.
            .scope = pack.packScopeKey(0, 0),
            .kvmap_chunk = kvmap.chunkFromEnv(opts.env),
            .windows = undefined,
            .crossing_kill_after = knobs.intKnob("SQUEEZEFS_TEST_KVMAP_CROSSING_KILL_AFTER", u32, opts.env),
            .zcrx_registry = zcrx.steering.Registry.init(gpa),
        };
        self.shape = try ShapeTable.init(gpa);
        errdefer self.shape.deinit(gpa);
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

        // 4. Staging + the NVMe tier under the cache paths (or OFF, loud).
        var nvme: nvme_read_cache.Cache = nvme_read_cache.Cache.initDisabled(gpa);
        if (cfg.cache_paths.len > 0 and opened > 0) {
            try self.openStaging(gpa, io, cfg, opts);
            nvme = self.openNvmeTier(gpa, cfg, opts);
        } else {
            log.info("data plane: the set declares NO cache paths — staging and the NVMe read tier are OFF: a write ≤ inline_max ({d} B) stays inline, anything larger goes STRIPED (no packed layout, no promotion pass at dismount)", .{inline_max});
        }
        errdefer self.closeStaging();
        stats.data_staging_on.set(@intFromBool(self.stage != null));
        stats.data_nvme_tier_on.set(@intFromBool(nvme.enabled));

        // 5. The read path over the plane's seams.
        var rcfg = read_path.Config.fromEnv(opts.env, cfg.block_size, self.passthrough, opts.facts, opts.budget_bytes);
        if (opened == 0) rcfg.read_lane = false;
        const decode: ?read_path.Decode = if (self.xform != null) .{ .ctx = self, .chunk = cfg.chunk_size, .decode = &decodeImage } else null;
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
            .image = if (self.xform != null) seams.ImageSeam{ .ctx = self, .read = &imageSeamRead } else null,
        }) catch return error.OutOfMemory;
        errdefer self.wp.deinit(io);
        // The write side's R5 components (spec §2.5): the parked-buffer
        // pool's RAM (its cached cells return under the idle-trim law; the
        // drain-target shed is the write path's to bind) and the in-flight
        // DMA bytes (non-sheddable, drain by completion).
        budget.authority.register(self.wp.table.component(null));
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
        // The identity through THIS device (the discovery pass read it
        // through a scratch one): tag, set, geometry, transform.
        {
            const sector = dev.probeReadBlock(0, data_superblock.sector_len) catch return error.Io;
            defer sector.release();
            const fmt = data_superblock.classifySector0(sector.bytes(), null) catch return error.IdentityMismatch;
            const sb = switch (fmt) {
                .blank => return error.IdentityMismatch,
                .v1 => |sb| sb,
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
            crypto = transform.CryptoState.init(gpa, io, cfg.encryption, derived, cfg.nonce_floor, 1) catch return error.TransformRefused;
        }
        errdefer if (crypto) |c| c.deinit();
        const t = transform.Transform.init(gpa, opts.env, tcfg, crypto) catch return error.TransformRefused;
        const scratch_len: usize = @intCast(transform.scratchLen(cfg.block_size));
        const scratch = gpa.alignedAlloc(u8, .fromByteUnits(layout.GRAIN), scratch_len) catch return error.OutOfMemory;
        errdefer gpa.free(scratch);
        const read_scratch = gpa.alignedAlloc(u8, .fromByteUnits(layout.GRAIN), scratch_len) catch return error.OutOfMemory;
        self.xform = .{ .t = t, .crypto = crypto, .scratch = scratch, .read_scratch = read_scratch };
    }

    fn closeTransform(self: *DataPlane) void {
        if (self.xform) |*x| {
            x.t.deinit(self.gpa);
            if (x.crypto) |c| c.deinit();
            self.gpa.free(x.scratch);
            self.gpa.free(x.read_scratch);
            self.xform = null;
        }
    }

    /// The staging ring under the first cache path: the identity marker
    /// (bind / dead-wipe / fresh), the segment sized from the root's free
    /// space, the recovery scan seeding the pool's ledger.
    fn openStaging(self: *DataPlane, gpa: Allocator, io: Io, cfg: *const set_config.Config, opts: Options) OpenError!void {
        _ = io;
        const root = cfg.cache_paths[0];
        const root_fd = staging.Ring.openRoot(root) catch |e| {
            log.err("staging root {s}: cannot open ({s})", .{ root, @errorName(e) });
            return error.StagingRefused;
        };
        errdefer _ = linux.close(root_fd);
        const ours: staging.Marker = .{ .set_uuid = self.backend.setUuid(), .set_epoch = self.backend.setEpoch(), .node_token = 0, .mount_slot = 0 };
        const found = staging.Ring.readMarker(self.backend.pool, root) catch null;
        switch (staging.bindVerdict(found, ours, false)) {
            .absent, .match, .upgrade => {},
            .dead => staging.Ring.wipeSegments(root_fd, &.{STAGING_SEGMENT_NAME}),
            .foreign_scope => {
                log.err("staging root {s}: a PEER mount's live custody — refusing", .{root});
                return error.StagingRefused;
            },
        }
        staging.Ring.writeMarker(self.backend.pool, root, root_fd, ours) catch |e| {
            log.err("staging root {s}: cannot write the identity marker ({s})", .{ root, @errorName(e) });
            return error.StagingRefused;
        };
        const capacity = stagingCapacity(root_fd, opts.budget_bytes);
        var mapped = staging.Ring.openSegment(root_fd, STAGING_SEGMENT_NAME, capacity) catch |e| {
            log.err("staging root {s}: cannot open the segment ({s})", .{ root, @errorName(e) });
            return error.StagingRefused;
        };
        errdefer mapped.close();
        self.stage = .{ .root_fd = root_fd, .mapped = mapped, .seg = undefined, .pool = undefined };
        const st = &self.stage.?;
        st.seg = staging.Segment.init(gpa, st.mapped.map, st.mapped.backing()) catch return error.OutOfMemory;
        errdefer st.seg.deinit();
        const rep = st.seg.recover();
        st.pool = staging.Pool.init(gpa, capacity, .{ .ctx = self, .pressure = &promotionPressure, .promote = &promoteHook });
        st.pool.seedFromRecovery(&st.seg) catch return error.OutOfMemory;
        budget.authority.register(st.pool.component());
        log.info("staging ring {s}/{s}/{s}: {d} MiB, recovered {d} record(s) ({d} B), {d} torn, {d} superseded", .{ root, staging.Ring.SEGMENT_DIR, STAGING_SEGMENT_NAME, capacity >> 20, rep.adopted, rep.bytes, rep.torn, rep.superseded });
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

    fn openNvmeTier(self: *DataPlane, gpa: Allocator, cfg: *const set_config.Config, opts: Options) nvme_read_cache.Cache {
        var aggregate: u64 = 0;
        for (self.vols.rows[0..self.vols.n()]) |r| aggregate += r.?.alloc.capacityBlocks() * self.block_size;
        const tier_budget = nvme_read_cache.budgetFromEnv(opts.env, aggregate);
        if (tier_budget == 0) return nvme_read_cache.Cache.initDisabled(gpa);
        const shards = nvme_read_cache.shardPlan(tier_budget, opts.facts.cpus, self.chunk);
        const per_shard = derive.alignDown(tier_budget / @max(shards, 1), nvme_read_cache.GRAIN);
        if (per_shard < self.chunk + nvme_read_cache.GRAIN) return nvme_read_cache.Cache.initDisabled(gpa);
        return nvme_read_cache.Ring.open(gpa, cfg.cache_paths[0], shards, per_shard) catch |e| {
            log.err("NVMe read tier under {s}: cannot open ({s}) — the tier is OFF", .{ cfg.cache_paths[0], @errorName(e) });
            return nvme_read_cache.Cache.initDisabled(gpa);
        };
    }

    fn closeStaging(self: *DataPlane) void {
        if (self.stage) |*st| {
            st.pool.deinit();
            st.seg.deinit();
            st.mapped.close();
            _ = linux.close(st.root_fd);
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
    /// adopts it) and the pass is loud about it.
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
            const files0 = staging.stats.dismount_promoted_files.load();
            const bytes0 = staging.stats.dismount_promoted_bytes.load();
            const failed0 = staging.stats.dismount_promote_failures.load();
            staging.promoteAllAtDismount(self.gpa, &st.seg, st.pool.hooks) catch |e| log.err("dismount promotion pass: {s}", .{@errorName(e)});
            const failed = staging.stats.dismount_promote_failures.load() - failed0;
            if (failed != 0) {
                log.err("dismount promotion: {d} staged file(s) ({d} B) promoted, {d} FAILED — the failed residents stay in the staging ring (readable here, zeros elsewhere) until the next mount of this host promotes them", .{ staging.stats.dismount_promoted_files.load() - files0, staging.stats.dismount_promoted_bytes.load() - bytes0, failed });
            } else {
                log.info("dismount promotion: {d} staged file(s) ({d} B) promoted, 0 failed", .{ staging.stats.dismount_promoted_files.load() - files0, staging.stats.dismount_promoted_bytes.load() - bytes0 });
            }
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

    /// The filesystem publishes an inode's shape (every layout publish and
    /// size change); the queue-thread gates read it.
    pub fn noteShape(self: *DataPlane, ino: u64, size: u64, kind: layout.Kind) void {
        self.shape.publish(ino, size, kind == .striped and self.passthrough);
    }

    pub fn forgetShape(self: *DataPlane, ino: u64) void {
        self.shape.forget(ino);
    }

    /// The per-ino layout lock, level (3.7) (module doc). kv_fs takes it
    /// through the data face around its truncate / corpse transactions,
    /// BEFORE its 4a set and released after the commit landed; the plane's
    /// own sections take it inside.
    pub fn lockLayout(self: *DataPlane, io: Io, ino: u64) layout.LayoutLocks.Guard {
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

    /// THE read of any layout kind into `dest` (the reply window;
    /// `registered` = the device may DMA straight into it).
    pub fn read(self: *DataPlane, io: Io, ino: u64, offset: u64, dest: []u8, registered: bool, odirect: bool) Error!usize {
        return self.rp.read(io, ino, offset, dest.len, .{ .buf = dest, .registered = registered }, .{ .odirect = odirect });
    }

    /// ONE whole-block image landed on a fresh block (the promotion to
    /// striped / the own-block arm): placement by the router, the mint, the
    /// transform, the DMA — the proof minted by the completion. The caller
    /// publishes the layout naming `mapping` in its own transaction and
    /// calls `abandonOwnBlock` if that fails.
    pub fn ownBlock(self: *DataPlane, io: Io, ino: u64, image: []const u8) Error!OwnBlock {
        if (image.len > self.block_size) return error.FileTooBig;
        const tag = (self.rt.pickVolume() catch return error.OutOfMemory) orelse return error.NoSpaceLeft;
        const v = self.vols.byTag(tag) orelse return error.InputOutput;
        const minted = enospc.allocateWithValve(v.alloc, io, enospc.Valve.fromReclaimer(self.reclaimer)) catch |e| return switch (e) {
            error.StorageFull => error.NoSpaceLeft,
            error.ReadOnly => error.ReadOnlyFileSystem,
            error.PlaneRefused, error.NotFree => error.InputOutput,
        };
        var guard = block_alloc.MintedBlockGuard.init(v.alloc, minted);
        errdefer guard.abandon();
        const stored = try self.transformWrite(tag, image);
        self.deviceWrite(tag, minted.offset, stored) catch |e| return mapDma(e);
        v.alloc.publishBlock(minted.idx);
        guard.disarm();
        stats.own_block_dmas.inc();
        stats.own_block_dma_bytes.add(image.len);
        self.wp.touched.stampOrdinal(ino, v.ordinal);
        // The proof covers the SLOT the mapping names (`DmaProof` doc;
        // `publishLayout` checks `covers` over `slotLen` = the whole block
        // for a whole-block mapping). A sub-block image (an inline file
        // promoting at its first write past `inline_max`) lands only its
        // grain-padded bytes; the tail of a fresh mint is unnamed — every
        // read and every existing-bytes seed is bounded by the layout's
        // `size` — so the slot IS what the DMA proved. A grain-length proof
        // here refused every such promotion with ProofMissing (EIO on the
        // second write of a small file on a set without a cache path).
        return .{ .mapping = Mapping.wholeBlock(tag, minted.offset, minted.stamp), .proof = .{ .vol_tag = tag, .offset = minted.offset, .len = self.block_size, .completed_ns = clock.monotonicNs() }, .alloc = v.alloc, .idx = minted.idx };
    }

    /// The publish failed: the never-published mint returns.
    pub fn abandonOwnBlock(self: *DataPlane, ob: OwnBlock) void {
        _ = self;
        ob.alloc.abandonUnpublished(ob.idx);
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
        layout.publishLayout(t, .{ .ino = ino, .hash_seed = self.hash_seed, .coll_seq = rec.coll_seq, .prev = prev, .layout = next, .proofs = &.{proof} }, self.refsSeam(), self.geo) catch |e| {
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
    /// destroyed bindings through the reclaimer's ladder.
    pub fn freeBindings(self: *DataPlane, io: Io, bindings: []const Mapping) void {
        for (bindings) |b| {
            stats.bindings_freed.inc();
            // A tenant of a pack still OPEN leaves through the packer (the
            // pin arithmetic counts it); everything else runs the ladder.
            if (self.packer) |*p| if (p.releaseOpenTenant(b)) continue;
            _ = self.reclaim_free.seam().free(io, b);
        }
    }

    /// A truncate or destroy begins: every write-side custody of `ino`
    /// lands or drops, every detached upload of the mount lands — so the
    /// layout the caller clips is the whole truth.
    pub fn flushCustody(self: *DataPlane, io: Io, ino: u64) void {
        self.wp.settleCustody(io, ino);
    }

    /// RELEASE: the write-back class drains (parked blocks flush, the
    /// overlay drains, the epoch closes).
    pub fn releaseIno(self: *DataPlane, io: Io, ino: u64) void {
        self.wp.flushIno(io, ino);
    }

    /// fsync (charter §6.4) through `fsync.zig`'s legs; the meta barrier is
    /// the backend's coalesced barrier.
    pub fn fsync(self: *DataPlane, io: Io, ino: u64, staged_key: ?staging.StagingKey) Error!void {
        var ctx = self.wp.fsyncCtx();
        ctx.meta_sync = .{ .ctx = self, .sync = &metaSync };
        ctx.staged_key = staged_key;
        if (self.stage) |*st| ctx.promotion = st.pool.hooks;
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
        return s.generation;
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

    /// The ring entry leaves (a promotion landed, a re-stage superseded it,
    /// an unlink, a layout that moved away): generation-gated when known
    /// (null = whatever copy is resident — the layout no longer names it).
    pub fn releaseStaged(self: *DataPlane, key_: staging.StagingKey, generation: ?u64) void {
        const st = &(self.stage orelse return);
        if (generation) |g| {
            _ = staging.releaseStaged(&st.pool, &st.seg, key_, g);
            return;
        }
        if (st.seg.remove(key_, null)) _ = st.pool.credit(key_.file_id, null);
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
        try self.deviceRead(w.vol_tag, w.device_off, out[0..@intCast(w.len)]);
        if (!v.alloc.fillIncarnationStill(v.alloc.idxOf(m.offset), before)) return error.InputOutput;
        if (self.xform != null) {
            // A tenant image is a transform frame: decode into the plane's
            // read scratch, then the plaintext view.
            const x = &self.xform.?;
            x.lock.lockUncancelable(self.io);
            defer x.lock.unlock(self.io);
            const plain = x.t.processRead(out[0..@intCast(w.len)], x.read_scratch) catch return error.InputOutput;
            if (plain.len > out.len) return error.BufferTooSmall;
            std.mem.copyForwards(u8, out[0..plain.len], plain);
            return out[0..plain.len];
        }
        return out[w.lo..w.hi];
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

    /// The ONE primitive (charter §6.1): dispatch on size — fits inline ⇒
    /// inline; `slot > CHUNK/2` ⇒ its own block; else a packed tenant. The
    /// identity is re-checked under the promotion's locks — the (3.5) stripe
    /// THEN the (3.7) layout lock (module doc; `PublishLocks.lockPromotion`)
    /// — (still staged, same file id, same ring generation); true iff the
    /// entry left the ring. A displaced durable copy is released after the
    /// locks drop (RES-1).
    pub fn promoteStaged(self: *DataPlane, k: staging.StagingKey) bool {
        const io = self.io;
        const st = &(self.stage orelse return false);
        const ino = k.ino();
        stats.promotions_run.inc();
        const lease = st.seg.get(k) orelse {
            stats.promotions_stale.inc();
            return false;
        };
        defer lease.release();
        const generation = lease.generation();
        const image = lease.bytes();
        const locks = self.locks.lockPromotion(io, ino);
        const out = self.promoteLocked(io, ino, k, generation, image);
        locks.release(io);
        if (out.displaced) |d| if (self.packer) |*p| p.releaseTenantRef(d);
        return out.ok;
    }

    const Promoted = struct { ok: bool, displaced: ?Mapping = null };

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
            return .{ .ok = false };
        };
        defer self.gpa.free(cur_bytes.value);
        const cur = layout.XattrLayout.decode(cur_bytes.value, self.geo) catch |e| {
            log.debug("promotion ino {d}: the layout record failed to decode ({t})", .{ ino, e });
            return .{ .ok = false };
        };
        if (cur.kind != .staged or !std.mem.eql(u8, &cur.file_id, &k.file_id) or st.seg.residentGeneration(k) != generation) {
            stats.promotions_stale.inc();
            log.debug("promotion ino {d}: stale — layout kind {t}, file id {s}, resident generation {?d} vs leased {d}", .{ ino, cur.kind, if (std.mem.eql(u8, &cur.file_id, &k.file_id)) "same" else "DIFFERENT", st.seg.residentGeneration(k), generation });
            return .{ .ok = false };
        }
        const policy = if (self.packer) |*p| p.policy else pack.Policy{ .enabled = false, .max_slot = self.chunk / 2 };
        const stored_len: u64 = if (self.xform != null) blk: {
            const x = &self.xform.?;
            x.lock.lockUncancelable(io);
            defer x.lock.unlock(io);
            const img = x.t.processWrite(image, x.scratch) catch return .{ .ok = false };
            break :blk img.len;
        } else image.len;
        const d = pack.dispatch(policy, cur.size, stored_len, self.inline_max, self.chunk) catch |e| {
            stats.promotions_failed.inc();
            log.debug("promotion ino {d}: no arm for size {d} / stored {d} ({t})", .{ ino, cur.size, stored_len, e });
            return .{ .ok = false };
        };
        const out: Promoted = switch (d) {
            .inline_ => .{ .ok = self.promoteInline(ino, cur, cur_bytes.coll_seq, image, k, generation) },
            .own_block => .{ .ok = self.promoteOwnBlock(io, ino, cur, cur_bytes.coll_seq, image, k, generation) },
            .packed_ => self.promotePacked(io, ino, cur_bytes.coll_seq, image, k, generation),
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
        layout.publishLayout(t, .{ .ino = ino, .hash_seed = self.hash_seed, .coll_seq = coll_seq, .prev = prev, .layout = layout.Layout.inlineData(prev.size, data), .proofs = &.{} }, self.refsSeam(), self.geo) catch {
            self.backend.abandon(t);
            return false;
        };
        self.backend.commit(t) catch return false;
        self.releaseStaged(k, generation);
        pack.stats.layout_promoted_inline.inc();
        return true;
    }

    fn promoteOwnBlock(self: *DataPlane, io: Io, ino: u64, prev: layout.Layout, coll_seq: u8, image: []const u8, k: staging.StagingKey, generation: u64) bool {
        const ob = self.ownBlock(io, ino, image) catch |e| {
            log.debug("promotion ino {d}: own block refused ({t})", .{ ino, e });
            return false;
        };
        const entries = [_]layout.MapEntryWire{(layout.MapEntry{ .block = 0, .mapping = ob.mapping }).encode()};
        const t = self.backend.begin(@src()) catch {
            self.abandonOwnBlock(ob);
            return false;
        };
        layout.publishLayout(t, .{ .ino = ino, .hash_seed = self.hash_seed, .coll_seq = coll_seq, .prev = prev, .layout = layout.Layout.stripedInline(prev.size, &entries), .proofs = &.{ob.proof} }, self.refsSeam(), self.geo) catch {
            self.backend.abandon(t);
            self.abandonOwnBlock(ob);
            return false;
        };
        self.backend.commit(t) catch {
            self.abandonOwnBlock(ob);
            return false;
        };
        self.releaseStaged(k, generation);
        pack.stats.layout_promoted_block.inc();
        return true;
    }

    /// The packed arm under the caller's (3.5) + (3.7): `pack.commitLocked`
    /// re-checks the identity and commits; its displaced copy rides back
    /// for the release after the locks drop.
    fn promotePacked(self: *DataPlane, io: Io, ino: u64, coll_seq: u8, image: []const u8, k: staging.StagingKey, generation: u64) Promoted {
        const p = &(self.packer orelse return .{ .ok = false });
        const st = &self.stage.?;
        const tag = (self.rt.pickVolume() catch return .{ .ok = false }) orelse {
            log.debug("promotion ino {d}: no data volume to pack onto", .{ino});
            return .{ .ok = false };
        };
        const stored = self.transformWrite(tag, image) catch |e| {
            log.debug("promotion ino {d}: the transform refused ({t})", .{ ino, e });
            return .{ .ok = false };
        };
        const prepared = (p.prepare(io, .{ .vol_tag = tag, .scope = self.scope }, stored, clock.monotonicNs()) catch |e| {
            log.debug("promotion ino {d}: pack prepare refused ({t})", .{ ino, e });
            return .{ .ok = false };
        }) orelse {
            log.debug("promotion ino {d}: the pack arm is stopped", .{ino});
            return .{ .ok = false };
        };
        const t = self.backend.begin(@src()) catch {
            return .{ .ok = false };
        };
        const outcome = p.commitLocked(prepared, .{
            .ino = ino,
            .hash_seed = self.hash_seed,
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
            .stale, .failed => |_, tag_| blk: {
                self.backend.abandon(t);
                log.debug("promotion ino {d}: the pack commit answered {t}", .{ ino, tag_ });
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

    fn packAllocatePlaced(ctx: *anyopaque, _: u64) pack.AllocError!pack.Placed {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const tag = (self.rt.pickVolume() catch return error.OutOfMemory) orelse return error.StorageFull;
        const v = self.vols.byTag(tag) orelse return error.VolumeFailed;
        const m = enospc.allocateWithValve(v.alloc, self.io, enospc.Valve.fromReclaimer(self.reclaimer)) catch |e| return switch (e) {
            error.StorageFull => error.StorageFull,
            error.ReadOnly, error.PlaneRefused, error.NotFree => error.VolumeFailed,
        };
        return .{ .vol_tag = tag, .offset = m.offset, .incarnation = m.stamp };
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
        self.deviceRead(vol_tag, device_off, out) catch |e| return switch (e) {
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
    }

    /// Any window: aligned straight into `out`, else bounced through a
    /// pooled read of the enclosing grain window.
    fn deviceRead(self: *DataPlane, vol_tag: u64, offset: u64, out: []u8) Error!void {
        const dev = self.deviceOf(vol_tag) orelse return error.VolumeFailed;
        if (out.len == 0) return;
        if (derive.isAligned(offset, layout.GRAIN) and derive.isAligned(out.len, layout.GRAIN) and derive.isAligned(@intFromPtr(out.ptr), layout.GRAIN)) {
            dev.readInto(offset, out) catch |e| return mapRead(e);
            return;
        }
        stats.device_read_bounces.inc();
        const start = derive.alignDown(offset, layout.GRAIN);
        const end = derive.alignUp(offset + out.len, layout.GRAIN);
        const buf = dev.readBlock(start, @intCast(end - start)) catch |e| return mapRead(e);
        defer buf.release();
        @memcpy(out, buf.bytes()[@intCast(offset - start)..][0..out.len]);
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
        self.deviceRead(vol_tag, offset, out) catch |e| return switch (e) {
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

    /// The stored image of `plain` (the plane's serialized transform).
    pub fn transformWrite(self: *DataPlane, vol_tag: u64, plain: []const u8) Error![]const u8 {
        _ = vol_tag;
        const x = &(self.xform orelse return plain);
        x.lock.lockUncancelable(self.io);
        defer x.lock.unlock(self.io);
        return x.t.processWrite(plain, x.scratch) catch return error.InputOutput;
    }

    fn transformSeam(self: *DataPlane) accumulate.TransformSeam {
        if (self.xform == null) return accumulate.TransformSeam.passthrough;
        return .{ .ctx = self, .process_write = &seamProcessWrite, .scratch_len = &seamScratchLen };
    }

    /// `seams.ImageSeam` on a transformed set: the write side's seeds read
    /// the block's PLAINTEXT image (`readMappingImage` decodes the frame),
    /// never the raw frame bytes.
    fn imageSeamRead(ctx: ?*anyopaque, b: Mapping, out: []u8) seams.ReadError!void {
        const self: *DataPlane = @ptrCast(@alignCast(ctx.?));
        const plain = self.readMappingImage(b, out) catch |e| return switch (e) {
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
        x.lock.lockUncancelable(self.io);
        defer x.lock.unlock(self.io);
        return x.t.processWrite(plain, scratch);
    }

    fn seamScratchLen(ctx: ?*anyopaque, _: u64) usize {
        const self: *DataPlane = @ptrCast(@alignCast(ctx.?));
        return @intCast(transform.scratchLen(@intCast(self.block_size)));
    }

    fn decodeImage(ctx: *anyopaque, image: []u8, out: []u8) error{Corrupt}![]const u8 {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const x = &self.xform.?;
        x.lock.lockUncancelable(self.io);
        defer x.lock.unlock(self.io);
        return x.t.processRead(image, out) catch return error.Corrupt;
    }

    // ---- the block-map authority (seams.MapSeam) ----

    pub fn mapSeam(self: *DataPlane) seams.MapSeam {
        return .{ .ctx = self, .vtable = &map_vtable };
    }

    const map_vtable: seams.MapSeam.VTable = .{ .lookup = &mapLookup, .view = &mapView, .merge = &mapMerge, .current_fencing = &mapFencing };

    /// The layout record of `ino`: the xattr chain at the layout name's
    /// hash, the record named `layout` (owned bytes) and its chain slot.
    /// A FOREIGN slot's record comes under its holder's token (charter
    /// §5.5: tokens are the ONLY foreign-read method; the grant carries
    /// the carried xattrs, `layout` among them), never off this mount's
    /// projection.
    const LayoutRecord = struct { value: []u8, coll_seq: u8 };

    pub fn layoutRecord(self: *DataPlane, ino: u64) Error!?LayoutRecord {
        if (self.foreign_layout) |f| switch (try f.lookup(f.ctx, self.gpa, ino)) {
            .local => {},
            .gone => return null,
            // A token page's chain slot is the holder's; a foreign record
            // is read, never republished from here (the publish ships).
            .foreign => |x| return if (x.layout) |v| .{ .value = v, .coll_seq = 0 } else null,
        };
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(self.gpa, &kvs);
        const h = key.xattrNameHash56(layout.LAYOUT_XATTR_NAME, self.hash_seed);
        const start = key.xattrKey(ino, h, 0);
        const end = key.xattrKey(ino, h, std.math.maxInt(u8));
        self.backend.range(self.gpa, key.TREE_XATTRS, &start, &end, 256, &kvs) catch |e| return KvBackend.coreError(e);
        for (kvs.items) |kv| {
            const xv = record.XattrValue.decode(kv.value) catch return error.InputOutput;
            if (std.mem.eql(u8, xv.name, layout.LAYOUT_XATTR_NAME)) {
                return .{ .value = try self.gpa.dupe(u8, kv.value), .coll_seq = kv.key[kv.key.len - 1] };
            }
        }
        return null;
    }

    fn inodeSize(self: *DataPlane, ino: u64) Error!?u64 {
        if (self.foreign_layout) |f| switch (try f.lookup(f.ctx, self.gpa, ino)) {
            .local => {},
            .gone => return null,
            .foreign => |x| {
                if (x.layout) |v| self.gpa.free(v);
                return x.size;
            },
        };
        const bytes = (self.backend.lookup(self.gpa, key.TREE_INODES, &key.inodeKey(ino)) catch |e| return KvBackend.coreError(e)) orelse return null;
        defer self.gpa.free(bytes);
        const v = record.InodeValue.decode(bytes) catch return error.InputOutput;
        return v.size;
    }

    fn mapLookup(ctx: *anyopaque, ino: u64, block: u32) ?Mapping {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const rec = (self.layoutRecord(ino) catch return null) orelse return null;
        defer self.gpa.free(rec.value);
        const l = layout.XattrLayout.decode(rec.value, self.geo) catch return null;
        if (l.kind != .striped) return null;
        if (!l.isKvmap()) return l.lookup(self.chunk, block);
        return self.kvmapLookup(ino, l, block);
    }

    /// The tree-mapped resolve ladder: an index at or above a live sweep
    /// cursor is a HOLE (shadowed residue); then the window store's span
    /// verdict (a filled span's miss is a definitive hole); then the tree —
    /// ONE range page filled into the store. An unresolvable record is loud
    /// (`kvmap_resolve_failures`), never a fabricated hole.
    fn kvmapLookup(self: *DataPlane, ino: u64, head: layout.Layout, block: u32) ?Mapping {
        if (head.kvmapHead().?.sweep) |k| if (block >= k) return null;
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
        const size = (self.inodeSize(ino) catch return null) orelse return null;
        const rec = (self.layoutRecord(ino) catch return null) orelse return .{ .kind = .inline_, .size = size, .map_resident = true };
        defer self.gpa.free(rec.value);
        const l = layout.XattrLayout.decode(rec.value, self.geo) catch return null;
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
                if (!old.sameRef(inserts[j].binding, chunk) and out.n_displaced < seams.MergeOutcome.max_displaced) {
                    out.displaced[out.n_displaced] = old;
                    out.n_displaced += 1;
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
    fn mapMerge(ctx: *anyopaque, req: seams.MergeRequest) seams.MergeError!seams.MergeOutcome {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const io = self.io;
        const gpa = self.gpa;
        if (req.fencing != 1) return error.FencingTokenExpired;
        const lock = self.lockLayout(io, req.ino);
        defer lock.release(io);
        const rec = (self.layoutRecord(req.ino) catch return error.Transient);
        defer if (rec) |r| gpa.free(r.value);
        var prev: ?layout.Layout = null;
        var coll_seq: u8 = 0;
        if (rec) |r| {
            prev = layout.XattrLayout.decode(r.value, self.geo) catch return error.Transient;
            coll_seq = r.coll_seq;
            // A binding for a file that is no longer striped belongs to a
            // dead generation (a truncate to zero re-classed it): discard.
            if (prev.?.kind != .striped) return error.NotFound;
            if (prev.?.isKvmap()) return self.kvmapMerge(req, prev.?, coll_seq);
        } else {
            // The inode must still exist: a destroyed ino takes no map.
            if ((self.inodeSize(req.ino) catch return error.Transient) == null) return error.NotFound;
        }
        const old_entries: []const layout.MapEntryWire = if (prev) |p| p.entries else &.{};
        var merged: std.ArrayList(layout.MapEntryWire) = .empty;
        defer merged.deinit(gpa);
        var out: seams.MergeOutcome = .{};
        foldInserts(gpa, old_entries, req.inserts, self.chunk, &merged, &out) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Corrupt => error.Transient,
        };
        if (merged.items.len > self.inlineMapCap()) return self.crossing(req, prev.?, coll_seq, merged.items, &out);
        const size = @max(if (prev) |p| p.size else 0, req.min_size);
        const new = layout.Layout.stripedInline(size, merged.items);
        const t = self.backend.begin(@src()) catch return error.OutOfMemory;
        layout.publishLayout(t, .{ .ino = req.ino, .hash_seed = self.hash_seed, .coll_seq = coll_seq, .prev = prev, .layout = new, .proofs = req.proofs }, self.refsSeam(), self.geo) catch |e| {
            self.backend.abandon(t);
            stats.layout_map_merge_failures.inc();
            log.err("ino {d}: the block-map publish of {d} binding(s) failed ({s}) — the upload keeps its custody and retries", .{ req.ino, req.inserts.len, @errorName(e) });
            return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                else => error.Transient,
            };
        };
        self.backend.commit(t) catch |e| {
            stats.layout_map_merge_failures.inc();
            log.err("ino {d}: the block-map commit of {d} binding(s) failed ({s})", .{ req.ino, req.inserts.len, @errorName(e) });
            return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.VolumeFailed, error.VolumeClosed => error.WriterGuardFenced,
                else => error.Transient,
            };
        };
        stats.layout_map_merges.inc();
        stats.layout_map_merge_blocks.add(req.inserts.len);
        self.shape.publish(req.ino, size, self.passthrough);
        return out;
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
                layout.publishLayout(t, .{ .ino = ino, .hash_seed = self.hash_seed, .coll_seq = coll_seq, .prev = prev, .layout = head, .proofs = req.proofs, .arm = .{ .flip_to_kvmap = .{ .moved = prev.entries.len } } }, self.refsSeam(), self.geo) catch |e| {
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
        self.shape.publish(ino, size, self.passthrough);
        log.info("ino {d}: block-map-tree crossing — {d} inline entries → {d} records ({d} ops over {d} tx), {d} preexisting record(s) reconciled; head flipped at size {d}, gen {d}", .{ ino, merged.len, plan.records, plan.ops.len, chunks + 1, plan.preexisting, size, kvmap.FIRST_MAP_GEN });
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
        var barrier_frees: std.ArrayList(Mapping) = .empty;
        defer barrier_frees.deinit(gpa);
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
            layout.publishLayout(t, .{ .ino = ino, .hash_seed = self.hash_seed, .coll_seq = coll_seq, .prev = prev, .layout = layout.Layout.stripedKvmap(size, cursor, head.gen orelse kvmap.FIRST_MAP_GEN), .proofs = &.{} }, self.refsSeam(), self.geo) catch |e| {
                self.backend.abandon(t);
                stats.layout_map_merge_failures.inc();
                return switch (e) {
                    error.OutOfMemory => error.OutOfMemory,
                    else => error.Transient,
                };
            };
        }
        self.backend.commit(t) catch |e| {
            stats.layout_map_merge_failures.inc();
            log.err("ino {d}: the block-map commit of {d} binding(s) failed ({s})", .{ ino, window.len, @errorName(e) });
            return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.VolumeFailed, error.VolumeClosed => error.WriterGuardFenced,
                else => error.Transient,
            };
        };
        stats.layout_map_merges.inc();
        stats.layout_map_merge_blocks.add(window.len);
        kvmap.stats.kvmap_window_saves.inc();
        self.windows.invalidate(ino);
        self.shape.publish(ino, size, self.passthrough);
        // The barrier's frees: the caller holds its (3.5) stripe across this
        // merge, so they run on the meta pool after it returns (RES-1).
        if (barrier_frees.items.len != 0) self.deferFrees(barrier_frees.toOwnedSlice(gpa) catch return out);
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
            layout.publishLayout(t, .{ .ino = ino, .hash_seed = self.hash_seed, .coll_seq = rec.coll_seq, .prev = prev, .layout = layout.Layout.stripedKvmap(prev.size, cursor_after, prev.kvmapHead().?.gen orelse kvmap.FIRST_MAP_GEN), .proofs = &.{} }, self.refsSeam(), self.geo) catch {
                self.backend.abandon(t);
                return error.Transient;
            };
            self.backend.commit(t) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.VolumeFailed, error.VolumeClosed => error.WriterGuardFenced,
                else => error.Transient,
            };
            kvmap.stats.map_sweep_chunks.inc();
            self.windows.invalidate(ino);
            for (page.freed.items) |f| try frees.append(gpa, f.binding);
            if (next == null) break;
            start = next.?;
        }
        return end;
    }

    fn deferFrees(self: *DataPlane, bindings: []Mapping) void {
        self.tasks.spawn(self.io, deferredFreeTask, .{ self, bindings }) catch {
            // Never leaked: the frees run here (the one RES-1 exception the
            // executor's refusal forces).
            self.freeBindings(self.io, bindings);
            self.gpa.free(bindings);
        };
    }

    fn deferredFreeTask(self: *DataPlane, bindings: []Mapping) Io.Cancelable!void {
        self.freeBindings(self.io, bindings);
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
        layout.publishLayout(t, .{ .ino = ino, .hash_seed = self.hash_seed, .coll_seq = coll_seq, .prev = prev, .layout = layout.Layout.stripedKvmap(new_size, cursor, head.gen orelse kvmap.FIRST_MAP_GEN), .proofs = &.{} }, self.refsSeam(), self.geo) catch |e| return switch (e) {
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
            self.backend.commit(t) catch |e| return KvBackend.coreError(e);
            kvmap.stats.map_sweep_chunks.inc();
            if (outcome == .done and nlink == 0 and inode != null) kvmap.stats.kvmap_corpses_destroyed.inc();
            self.windows.invalidate(ino);
            try frees.ensureTotalCapacity(gpa, page.freed.items.len);
            for (page.freed.items) |f| frees.appendAssumeCapacity(f.binding);
        }
        self.freeBindings(io, frees.items);
        return outcome;
    }

    fn publishHead(self: *DataPlane, t: *tx_mod.KvTx, ino: u64, coll_seq: u8, prev: layout.Layout, new: layout.Layout) Error!void {
        layout.publishLayout(t, .{ .ino = ino, .hash_seed = self.hash_seed, .coll_seq = coll_seq, .prev = prev, .layout = new, .proofs = &.{} }, self.refsSeam(), self.geo) catch |e| return switch (e) {
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
        return .{ .ctx = self, .file = &metaFile, .binding = &metaBinding, .inline_data = &metaInlineData };
    }

    fn metaFile(ctx: *anyopaque, ino: u64) ?read_path.FileInfo {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const size = (self.inodeSize(ino) catch return null) orelse return null;
        const rec = (self.layoutRecord(ino) catch return null) orelse return .{ .size = size, .kind = .inline_ };
        defer self.gpa.free(rec.value);
        const l = layout.XattrLayout.decode(rec.value, self.geo) catch return null;
        return .{ .size = size, .kind = l.kind, .file_id = l.file_id, .durable_copy = l.durableCopy() };
    }

    fn metaBinding(ctx: *anyopaque, ino: u64, block: u64) ?Mapping {
        if (block > std.math.maxInt(u32)) return null;
        return mapLookup(ctx, ino, @intCast(block));
    }

    fn metaInlineData(ctx: *anyopaque, ino: u64, offset: u64, out: []u8) usize {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
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
        try self.deviceRead(vol_tag, device_off, dest);
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
        return accumulate.stats.write_through_bytes.load() +% patch_w1.stats.patch_write_bytes.load() +% device_overlay.stats.overlay_store_bytes.load() +% read_path.stats.ranged_read_bytes.load() +% read_path.stats.read_fill_dma_bytes.load() +% stats.own_block_dma_bytes.load();
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
        self.deviceRead(w.vol_tag, w.device_off + w.lo, out[0 .. w.hi - w.lo]) catch {};
    }

    /// This client's ring entry for a staged file.
    fn wsStagedPayload(ctx: *anyopaque, ino: u64, file_id: layout.FileId, offset: u64, len: u64, dest: []u8) read_path.StagedVerdict {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const st = &(self.stage orelse return .absent);
        const k = staging.StagingKey.init(file_id, ino);
        if (st.seg.get(k)) |lease| {
            defer lease.release();
            const bytes = lease.bytes();
            const want: usize = @intCast(@min(len, dest.len));
            @memset(dest[0..want], 0);
            if (offset < bytes.len) {
                const n = @min(want, bytes.len - offset);
                @memcpy(dest[0..n], bytes[@intCast(offset)..][0..n]);
            }
            return .{ .served = want };
        }
        if (st.pool.generationOf(file_id) != null) return .in_flight;
        return .absent;
    }

    fn wsHasRetainedExtents(ctx: *anyopaque, ino: u64) bool {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        var blocks: [64]u32 = undefined;
        for (self.wp.table.blocksOf(ino, &blocks)) |b| {
            if (self.wp.table.get(ino, b)) |e| if (e.repr == .extent) return true;
        }
        return false;
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

    /// The W1 hold gate: a PURE predicate over the hint and the published
    /// shape (striped on a passthrough set, non-extending, within one
    /// block, ≤ patch_max, not stream-adjacent) — no lookup, no lock.
    pub fn writeHoldEligible(self: *DataPlane, hint: fsv.HoldHint) bool {
        const snap = self.shape.peek(hint.ino) orelse return declineHold();
        if (!snap.striped_passthrough or self.held_store == null) return declineHold();
        const end = hint.offset + hint.len;
        if (end > snap.size or hint.len > self.wp.cfg.patch_max) return declineHold();
        if (hint.offset / self.block_size != (end - 1) / self.block_size) return declineHold();
        if (self.wp.last_write_end.peek(hint.ino) == hint.offset) return declineHold();
        stats.hold_gate_admits.inc();
        return true;
    }

    fn declineHold() bool {
        stats.hold_gate_declines.inc();
        return false;
    }

    /// The shared-mode admission probe (charter §6.2): a resident striped
    /// snapshot whose size the write stays within. `nodeid` is the
    /// kernel's; the table is keyed by the plane's effective ino.
    pub fn sharedProbe(self: *DataPlane, nodeid: u64, offset: u64, end: u64) ?op_guard.SharedProbe {
        _ = offset;
        const snap = self.shape.peek(shapeProbeKey(nodeid) orelse return null) orelse return null;
        if (!snap.striped_passthrough and self.passthrough) return null;
        return .{ .size_floor = snap.size, .meta_size = snap.size, .block_size = self.block_size, .range_fully_mapped = null, .narrow = end <= snap.size };
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
        return !self.wp.writeSideView().hasCustody(ino, block_index);
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
            var report = block_refs.verifyRefs(gpa, self.refs_store, v.vol_tag, owners.items, .{ .ctx = self, .isOpenPack = &isOpenPack, .isInflight = &isInflight }) catch |e| {
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

    fn isOpenPack(ctx: ?*anyopaque, block_idx: u64) bool {
        const self: *DataPlane = @ptrCast(@alignCast(ctx.?));
        const p = &(self.packer orelse return false);
        for (self.vols.rows[0..self.vols.n()]) |r| {
            if (p.openPack(.{ .vol_tag = r.?.vol_tag, .scope = self.scope })) |op| if (op.base / self.chunk == block_idx) return true;
        }
        return false;
    }

    fn isInflight(ctx: ?*anyopaque, block_idx: u64) bool {
        const self: *DataPlane = @ptrCast(@alignCast(ctx.?));
        for (self.vols.rows[0..self.vols.n()]) |r| if (r.?.alloc.inflightContains(block_idx)) return true;
        return false;
    }

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
        try w.print(",\"transform\":{{\"compress_stored_raw\":{d},\"crypto_key_unwraps\":{d},\"crypto_key_unwrap_hits\":{d},\"crypto_unwrap_cache_full\":{d}}}", .{ transform.stats.compress_stored_raw.load(), transform.stats.crypto_key_unwraps.load(), transform.stats.crypto_key_unwrap_hits.load(), transform.stats.crypto_unwrap_cache_full.load() });
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

test "the shape table publishes ino-last and a torn row never matches a foreign ino" {
    var t = try ShapeTable.init(testing.allocator);
    defer t.deinit(testing.allocator);
    try testing.expect(t.peek(7) == null);
    t.publish(7, 4096, true);
    const s = t.peek(7).?;
    try testing.expectEqual(@as(u64, 4096), s.size);
    try testing.expect(s.striped_passthrough);
    try testing.expect(t.peek(8) == null);
    t.forget(7);
    try testing.expect(t.peek(7) == null);
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

test "stats JSON names every plane instrument" {
    var buf: [4096]u8 = undefined;
    var w: Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    const s = w.buffered();
    inline for (.{ "\"data_volumes\":", "\"layout_map_merges\":", "\"kvmap_sweep_spawn_refusals\":", "\"held_stores\":", "\"hold_gate_admits\":", "\"prefetch_fibers\":", "\"bindings_freed\":", "\"data_unresolved_volume\":", "\"kvmap_head_put_unlocked\":" }) |k| {
        try testing.expect(std.mem.indexOf(u8, s, k) != null);
    }
}
