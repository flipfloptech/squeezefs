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
//! Locks (charter §2): the plane's per-ino LAYOUT lock serializes every
//! layout-record read→stage→commit (the merge, kv_fs's inline/staged/
//! truncate/destroy transactions) — a PARKING mutex (the commit parks),
//! 4096 stripes at the 4a load factor; it nests INSIDE the write side's
//! (3.5) meta stripe when a core holds one and is taken alone by the epoch
//! close and kv_fs. Never held across a terminal block free (RES-1): every
//! displaced binding is freed by the caller after its transaction landed.
//!
//! What this wave does NOT do (recorded in `docs/evidence/rung-04-data-
//! plane.md`): the block-map tree crossing (a striped file past the inline
//! map's record cap is refused `EFBIG`, counted); a RAM layout cache (the
//! map authority reads the KV record — latch-free through the node cache,
//! one decode per probe); the writeback spill of parked blocks into the
//! ring; per-lane transform contexts (one serialized `Transform`).

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
const taskset = @import("../sync/taskset.zig");
const nvme_dev = @import("../io/nvme_dev.zig");
const uring_fs = @import("../io/uring_fs.zig");
const backend_mod = @import("../meta/backend.zig");
const key = @import("../meta/key.zig");
const record = @import("../meta/record.zig");
const tree = @import("../meta/tree.zig");
const tx_mod = @import("../meta/tx.zig");
const dlm = @import("../meta/dlm.zig");
const op_guard = @import("../fuse/handler/op_guard.zig");
const fsv = @import("../fuse/transport/filesystem_vtable.zig");
const transport_slot = @import("../fuse/transport/slot.zig");
const layout = @import("layout.zig");
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

pub const KvBackend = backend_mod.KvBackend;
pub const Mapping = layout.Mapping;
pub const Error = errors.Error;
pub const BlockKey = read_size_class.BlockKey;

// ---------------------------------------------------------------------------
// Constants with their reasons.
// ---------------------------------------------------------------------------

/// The per-ino layout lock width: the 4a stripes' load factor (a collision
/// is one short commit-length section in 4096 at full handler parallelism).
pub const LAYOUT_LOCK_STRIPES: usize = 4096;
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
    /// `layout_kvmap_crossing_refused`: a striped map past the inline cap —
    /// the block-map tree crossing is owed; EFBIG, never a torn map.
    pub var layout_kvmap_crossing_refused: counters.ShardedCounter = .{};
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
    inline for (.{ "layout_map_merges", "layout_map_merge_blocks", "layout_map_merge_failures", "layout_kvmap_crossing_refused", "promotions_run", "promotions_stale", "promotions_failed", "own_block_dmas", "own_block_dma_bytes", "held_stores", "held_store_failures", "hold_gate_admits", "hold_gate_declines", "prefetch_fibers", "prefetch_fiber_refusals", "hold_ceremony_fibers", "bindings_freed", "device_read_bounces" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    try w.print("\"data_unresolved_volume\":{d}}}", .{counters.tripwireCount("data_unresolved_volume")});
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
    rt: router.Router,
    reclaimer: *reclaim.Reclaimer,
    reclaim_free: seams.ReclaimFree,
    stripes: layout.MetaStripes = .{},
    layout_locks: [LAYOUT_LOCK_STRIPES]sync_mutex.Mutex = @splat(.init),
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
        };
        self.shape = try ShapeTable.init(gpa);
        errdefer self.shape.deinit(gpa);
        self.rt = try router.Router.init(gpa, cfg.chunk_size);
        errdefer self.rt.deinit();

        // 1. Devices + allocators + the ledger seed.
        var opened: u32 = 0;
        errdefer self.closeVolumes(opened);
        for (cfg.data) |d| {
            if (d.state == .retired) continue;
            if (opened == router.MAX_VOLUMES) break;
            try self.openVolume(d, opts, &opened);
        }
        stats.data_volumes.set(opened);

        // 2. The reclaimer: every volume a target; the tier purge, the fence
        //    probe and the foreground signal; the worker + the drainer.
        self.reclaimer = reclaim.Reclaimer.init(gpa, reclaim.Config.fromEnv(opts.env, opts.budget_bytes)) catch return error.OutOfMemory;
        errdefer self.reclaimer.deinit();
        for (self.vols.rows[0..self.vols.count], 0..) |r, i| {
            const v = r.?;
            self.reclaimer.registerTarget(@intCast(i), v.alloc, reclaim.device_binding.target(self.devs[i].?));
        }
        self.reclaimer.setTierPurge(.{ .ctx = self, .purge = &reclaimPurge });
        self.reclaimer.setFenceProbe(.{ .ctx = self, .poll = &fencePoll });
        self.reclaimer.setForegroundSignal(.{ .ctx = null, .read = &foregroundBytes });
        for (self.vols.rows[0..self.vols.count]) |r| r.?.alloc.space_pending = self.reclaimer.spacePending();
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
            .stripes = &self.stripes,
            .mem = &budget.authority,
            .budget_bytes = opts.budget_bytes,
            .transform = self.transformSeam(),
            .scratch_len = if (self.xform != null) transform.scratchLen(cfg.block_size) else 0,
        }) catch return error.OutOfMemory;
        errdefer self.wp.deinit(io);

        // 7. The packer (the promotion primitive's packed arm).
        if (self.stage != null) {
            self.packer = pack.Packer.init(gpa, cfg.chunk_size, pack.Policy.fromEnv(opts.env, cfg.chunk_size), self.packAllocSeam(), self.dmaSeam(), self.packReadSeam(), router.MAX_VOLUMES) catch return error.OutOfMemory;
        }
        errdefer if (self.packer) |*p| p.deinit();

        // 8. The warm ladder (the probe arm); the zc legs arm with the session.
        self.ladder = .{ .rp = self.rp, .guards = opts.guards, .io = io, .ceremony = .{ .ctx = self, .spawn = &spawnCeremony } };
        for (self.vols.rows[0..self.vols.count]) |r| {
            const v = r.?;
            self.rt.updateCensus(v.vol_tag, v.alloc.usedBlocks() * cfg.block_size) catch {};
        }
        // The C8 oracle at mount (the A/B lever): the derived walk vs the ledger.
        if (block_refs.verifyAtMount(opts.env)) self.verifyRefsAtMount();
        log.info("data plane: {d} data volume(s), block {d} B, chunk {d} B, inline_max {d} B, inline map cap {d} entries, transform {s}/{s}, staging {s}, NVMe tier {s}, direct-store face pending the session", .{ opened, cfg.block_size, cfg.chunk_size, inline_max, self.inlineMapCap(), @tagName(cfg.compression), cfg.encryption.name(), if (self.stage != null) "ON" else "off", if (nvme.enabled) "ON" else "off" });
        return self;
    }

    fn openVolume(self: *DataPlane, d: set_config.DataVolume, opts: Options, opened: *u32) OpenError!void {
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
        const alloc = block_alloc.BlockAllocator.init(self.gpa, block_alloc.BlockAllocator.configFromEnv(opts.env, d.vol_tag, ordinal, self.chunk, capacity_bytes, false)) catch return error.OutOfMemory;
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
        self.rt.addVolume(.{ .id = .{ .tag = d.vol_tag }, .state = .active, .capacity_bytes = d.capacity_blocks * self.block_size, .used_bytes = alloc.usedBlocks() * self.block_size }) catch return error.OutOfMemory;
        self.vols.add(.{ .vol_tag = d.vol_tag, .ordinal = ordinal, .alloc = alloc, .passthrough = self.passthrough, .write_cache_volatile = dev.writeCache() == .write_back or dev.writeCache() == .unknown, .active = d.state == .active });
        self.devs[ordinal] = dev;
        opened.* += 1;
        log.info("data volume {s}: vol-{x:0>16} bound as ordinal {d}: {d} usable blocks, {d} referenced ({d} ref records), {d} free", .{ d.uri, d.vol_tag, ordinal, d.capacity_blocks, alloc.usedBlocks() -| 1, population orelse 0, alloc.freeBlocksCount() + alloc.virginBlocks() });
    }

    fn closeVolumes(self: *DataPlane, n: u32) void {
        var i: u32 = 0;
        while (i < n) : (i += 1) {
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
        const stamp = self.backend.mounted.membership_stamp;
        const ours: staging.Marker = .{ .set_uuid = stamp.set_uuid, .set_epoch = stamp.set_epoch, .node_token = 0, .mount_slot = 0 };
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
        for (self.vols.rows[0..self.vols.count]) |r| aggregate += r.?.alloc.capacityBlocks() * self.block_size;
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

    /// The dismount pass (charter §8 `umount`): the pending promotions and
    /// detached uploads drain; every staged file promotes; the open packs
    /// seal DISMOUNT; the reclaimer drains and stops; the paths, the
    /// devices and the allocators close — in that order, every step before
    /// the backend's final checkpoint.
    pub fn close(self: *DataPlane) void {
        const io = self.io;
        self.tasks.wait(io) catch {};
        self.wp.quiesce(io);
        if (self.stage) |*st| {
            const files0 = staging.stats.dismount_promoted_files.load();
            const bytes0 = staging.stats.dismount_promoted_bytes.load();
            const failed0 = staging.stats.dismount_promote_failures.load();
            staging.promoteAllAtDismount(self.gpa, &st.seg, st.pool.hooks) catch |e| log.err("dismount promotion pass: {s}", .{@errorName(e)});
            log.info("dismount promotion: {d} staged file(s) ({d} B) promoted, {d} failed", .{ staging.stats.dismount_promoted_files.load() - files0, staging.stats.dismount_promoted_bytes.load() - bytes0, staging.stats.dismount_promote_failures.load() - failed0 });
        }
        if (self.packer) |*p| p.sealAll();
        _ = self.reclaimer.drainSync(io);
        self.reclaimer.closeAtDismount();
        self.reclaimer.stopAndJoin(io);
        self.lane_group.await(io) catch {};
        self.rp.deinit();
        self.wp.deinit(io);
        if (self.packer) |*p| p.deinit();
        self.closeStaging();
        self.closeTransform();
        for (self.devs[0..self.vols.count]) |d| if (d) |dev| dev.closeAtDismount();
        self.closeVolumes(self.vols.count);
        self.reclaimer.deinit();
        self.rt.deinit();
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
        return self.vols.count != 0;
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

    /// The per-ino layout lock (module doc): kv_fs's layout transactions
    /// and the map merge serialize here.
    pub fn lockLayout(self: *DataPlane, io: Io, ino: u64) *sync_mutex.Mutex {
        const l = &self.layout_locks[@intCast(dlm.splitmix64(ino) & (LAYOUT_LOCK_STRIPES - 1))];
        l.lockUncancelable(io);
        return l;
    }

    /// Whether a HELD slot may ride the write path unmaterialized (the
    /// session installed the direct-store face).
    pub fn canStoreHeld(self: *const DataPlane) bool {
        return self.held_store != null;
    }

    /// THE striped write: the handler's level-1 admission passed through
    /// as `class`; `file` is the layout view the caller read.
    pub fn write(self: *DataPlane, io: Io, ino: u64, offset: u64, payload: write_admit.Payload, class: op_guard.Class, file: seams.FileView) write_admit.Error!write_admit.Outcome {
        return self.wp.write(io, .{ .ino = ino, .offset = offset, .payload = payload, .class = class, .file = file });
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
        return .{ .mapping = Mapping.wholeBlock(tag, minted.offset, minted.stamp), .proof = .{ .vol_tag = tag, .offset = minted.offset, .len = layout.ceilGrain(stored.len), .completed_ns = clock.monotonicNs() }, .alloc = v.alloc, .idx = minted.idx };
    }

    /// The publish failed: the never-published mint returns.
    pub fn abandonOwnBlock(self: *DataPlane, ob: OwnBlock) void {
        _ = self;
        ob.alloc.abandonUnpublished(ob.idx);
    }

    /// Terminal frees AFTER a landed transaction (RES-1): displaced and
    /// destroyed bindings through the reclaimer's ladder.
    pub fn freeBindings(self: *DataPlane, io: Io, bindings: []const Mapping) void {
        for (bindings) |b| {
            _ = self.reclaim_free.seam().free(io, b);
            stats.bindings_freed.inc();
        }
    }

    /// A truncate or destroy begins: every write-side custody of `ino`
    /// lands or drops, every detached upload of the mount lands — so the
    /// layout the caller clips is the whole truth.
    pub fn flushCustody(self: *DataPlane, io: Io, ino: u64) void {
        self.wp.flushIno(io, ino);
        self.wp.quiesce(io);
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

    /// `df` (charter §6): used = the bitmap population minus the writer's
    /// own unconsumed window — the free list, the virgin tail and the
    /// blocks inside open trim windows are all supply, never a deficit.
    /// Block 0 (the superblock chunk) is outside the capacity shown.
    pub fn statfs(self: *DataPlane) ?Space {
        if (self.vols.count == 0) return null;
        var total: u64 = 0;
        var free: u64 = 0;
        for (self.vols.rows[0..self.vols.count]) |r| {
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
    /// the generation the layout record's publish is gated on.
    pub fn stageImage(self: *DataPlane, key_: staging.StagingKey, image: []const u8) Error!u64 {
        const st = &(self.stage orelse return error.NotSupported);
        const s = staging.stageWrite(&st.pool, &st.seg, key_, image, 1) catch |e| return switch (e) {
            error.StorageFull => error.NoSpaceLeft,
            error.OutOfMemory => error.OutOfMemory,
        };
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
    /// identity is re-checked under the layout lock (still staged, same
    /// file id, same ring generation); true iff the entry left the ring.
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
        const lock = self.lockLayout(io, ino);
        defer lock.unlock(io);
        const cur_bytes = (self.layoutRecord(ino) catch return false) orelse {
            stats.promotions_stale.inc();
            return false;
        };
        defer self.gpa.free(cur_bytes.value);
        const cur = layout.XattrLayout.decode(cur_bytes.value, self.geo) catch return false;
        if (cur.kind != .staged or !std.mem.eql(u8, &cur.file_id, &k.file_id) or st.seg.residentGeneration(k) != generation) {
            stats.promotions_stale.inc();
            return false;
        }
        const policy = if (self.packer) |*p| p.policy else pack.Policy{ .enabled = false, .max_slot = self.chunk / 2 };
        const stored_len: u64 = if (self.xform != null) blk: {
            const x = &self.xform.?;
            x.lock.lockUncancelable(io);
            defer x.lock.unlock(io);
            const img = x.t.processWrite(image, x.scratch) catch return false;
            break :blk img.len;
        } else image.len;
        const d = pack.dispatch(policy, cur.size, stored_len, self.inline_max, self.chunk) catch {
            stats.promotions_failed.inc();
            return false;
        };
        const ok = switch (d) {
            .inline_ => self.promoteInline(io, ino, cur, cur_bytes.coll_seq, image, k, generation),
            .own_block => self.promoteOwnBlock(io, ino, cur, cur_bytes.coll_seq, image, k, generation),
            .packed_ => self.promotePacked(io, ino, cur, cur_bytes.coll_seq, image, k, generation),
        };
        if (ok) self.noteShape(ino, cur.size, if (d == .own_block) .striped else if (d == .inline_) .inline_ else .packed_) else stats.promotions_failed.inc();
        return ok;
    }

    fn promoteInline(self: *DataPlane, io: Io, ino: u64, prev: layout.Layout, coll_seq: u8, image: []const u8, k: staging.StagingKey, generation: u64) bool {
        _ = io;
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
        const ob = self.ownBlock(io, ino, image) catch return false;
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

    fn promotePacked(self: *DataPlane, io: Io, ino: u64, prev: layout.Layout, coll_seq: u8, image: []const u8, k: staging.StagingKey, generation: u64) bool {
        _ = prev;
        const p = &(self.packer orelse return false);
        const st = &self.stage.?;
        const tag = (self.rt.pickVolume() catch return false) orelse return false;
        const stored = self.transformWrite(tag, image) catch return false;
        const prepared = (p.prepare(io, .{ .vol_tag = tag, .scope = self.scope }, stored, clock.monotonicNs()) catch return false) orelse return false;
        const t = self.backend.begin(@src()) catch {
            return false;
        };
        const outcome = p.commit(io, prepared, .{
            .stripes = &self.stripes,
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
            .committed => true,
            .stale, .failed => blk: {
                self.backend.abandon(t);
                break :blk false;
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
    fn transformWrite(self: *DataPlane, vol_tag: u64, plain: []const u8) Error![]const u8 {
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
    const LayoutRecord = struct { value: []u8, coll_seq: u8 };

    pub fn layoutRecord(self: *DataPlane, ino: u64) Error!?LayoutRecord {
        var kvs: std.ArrayList(tree.KeyValue) = .empty;
        defer tree.deinitKeyValues(self.gpa, &kvs);
        const h = key.xattrNameHash56(layout.LAYOUT_XATTR_NAME, self.hash_seed);
        const start = key.xattrKey(ino, h, 0);
        const end = key.xattrKey(ino, h, std.math.maxInt(u8));
        self.backend.range(self.gpa, key.TREE_XATTRS, &start, &end, 256, &kvs) catch |e| return backend_mod.KvBackend.coreError(e);
        for (kvs.items) |kv| {
            const xv = record.XattrValue.decode(kv.value) catch return error.InputOutput;
            if (std.mem.eql(u8, xv.name, layout.LAYOUT_XATTR_NAME)) {
                return .{ .value = try self.gpa.dupe(u8, kv.value), .coll_seq = kv.key[kv.key.len - 1] };
            }
        }
        return null;
    }

    fn inodeSize(self: *DataPlane, ino: u64) Error!?u64 {
        const bytes = (self.backend.lookup(self.gpa, key.TREE_INODES, &key.inodeKey(ino)) catch |e| return backend_mod.KvBackend.coreError(e)) orelse return null;
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
        return l.lookup(self.chunk, block);
    }

    fn mapView(ctx: *anyopaque, ino: u64) ?seams.FileView {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const size = (self.inodeSize(ino) catch return null) orelse return null;
        const rec = (self.layoutRecord(ino) catch return null) orelse return .{ .kind = .inline_, .size = size, .map_resident = true };
        defer self.gpa.free(rec.value);
        const l = layout.XattrLayout.decode(rec.value, self.geo) catch return null;
        return .{ .kind = switch (l.kind) {
            .inline_ => .inline_,
            .staged => .staged,
            .packed_ => .packed_,
            .striped => .striped,
        }, .size = size, .map_resident = !l.isKvmap() };
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
        defer lock.unlock(io);
        const rec = (self.layoutRecord(req.ino) catch return error.Transient);
        defer if (rec) |r| gpa.free(r.value);
        var prev: ?layout.Layout = null;
        var coll_seq: u8 = 0;
        if (rec) |r| {
            prev = layout.XattrLayout.decode(r.value, self.geo) catch return error.Transient;
            coll_seq = r.coll_seq;
            // A binding for a file that is no longer striped belongs to a
            // dead generation (a truncate to zero re-classed it): discard.
            if (prev.?.kind != .striped or prev.?.isKvmap()) return error.NotFound;
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
        if (merged.items.len > self.inlineMapCap()) {
            stats.layout_kvmap_crossing_refused.inc();
            log.err("ino {d}: the inline block map would hold {d} entries (cap {d}) — the block-map tree crossing is owed; the publish is refused", .{ req.ino, merged.items.len, self.inlineMapCap() });
            return error.Transient;
        }
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

    fn readVolumes(self: *DataPlane) read_path.Volumes {
        return .{ .ctx = self, .allocator_of = &volAllocatorOf, .read_range = &volReadRange, .device_fd = &volDeviceFd };
    }

    fn volAllocatorOf(ctx: *anyopaque, vol_tag: u64) ?*block_alloc.BlockAllocator {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        const v = self.vols.byTag(vol_tag) orelse return null;
        return v.alloc;
    }

    fn volReadRange(ctx: *anyopaque, vol_tag: u64, device_off: u64, dest: []u8) read_path.DeviceError!void {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        return self.deviceRead(vol_tag, device_off, dest);
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
        for (self.devs[0..self.vols.count]) |d| if (d) |dev| if (dev.fenced()) return true;
        return false;
    }

    /// DEVICE-byte movement (never op counts): the manners law's signal.
    fn foregroundBytes(_: ?*anyopaque) u64 {
        return accumulate.stats.write_through_bytes.load() +% patch_w1.stats.patch_write_bytes.load() +% device_overlay.stats.overlay_store_bytes.load() +% read_path.stats.ranged_read_bytes.load() +% read_path.stats.read_fill_dma_bytes.load() +% stats.own_block_dma_bytes.load();
    }

    // ---- the read seam over the write side ----

    fn writeSide(self: *DataPlane) read_path.WriteSide {
        return .{ .ctx = self, .overlay_read = &wsOverlayRead, .extent_runs = &wsExtentRuns, .staged_block = &wsStagedBlock, .staged_payload = &wsStagedPayload, .has_retained_extents = &wsHasRetainedExtents, .open_overlay = &wsOpenOverlay };
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
    fn wsStagedBlock(ctx: *anyopaque, ino: u64, block: u64, rel_s: u64, rel_e: u64, dest: []u8) ?usize {
        const self: *DataPlane = @ptrCast(@alignCast(ctx));
        if (block > std.math.maxInt(u32)) return null;
        const b: u32 = @intCast(block);
        const view = self.wp.writeSideView();
        if (!view.hasCustody(ino, b)) return null;
        const want: usize = @intCast(rel_e - rel_s);
        const out = dest[0..want];
        @memset(out, 0);
        switch (view.serve(self.io, .{ .ino = ino, .block = b, .rel = @intCast(rel_s), .out = out })) {
            .served => return want,
            .partial => |p| {
                const base = p.base orelse return want;
                var runs: [cov.CoverageUnion.max_runs]cov.Run = undefined;
                const covered = p.covered.coveredRunsIn(@intCast(rel_s), @intCast(rel_e), &runs);
                var cursor: u32 = @intCast(rel_s);
                for (covered) |r| {
                    if (r.start > cursor) self.fillGap(base, cursor, out[cursor - @as(u32, @intCast(rel_s)) .. r.start - @as(u32, @intCast(rel_s))]);
                    cursor = r.end;
                }
                if (cursor < rel_e) self.fillGap(base, cursor, out[cursor - @as(u32, @intCast(rel_s)) ..]);
                return want;
            },
            .redirect => |shadow| {
                self.fillGap(shadow, @intCast(rel_s), out);
                return want;
            },
            .settle => {
                if (self.wp.overlay.registry.get(.{ .ino = ino, .block = b })) |rec| _ = self.wp.overlay.settleWriteArm(self.io, rec, false);
                return null;
            },
            .none => return null,
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
    /// snapshot whose size the write stays within.
    pub fn sharedProbe(self: *DataPlane, ino: u64, offset: u64, end: u64) ?op_guard.SharedProbe {
        _ = offset;
        const snap = self.shape.peek(ino) orelse return null;
        if (!snap.striped_passthrough and self.passthrough) return null;
        return .{ .size_floor = snap.size, .meta_size = snap.size, .block_size = self.block_size, .range_fully_mapped = null, .narrow = end <= snap.size };
    }

    pub fn probe(self: *DataPlane) op_guard.Probe {
        return .{ .ctx = self, .probe_fn = &probeFn };
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
        for (self.vols.rows[0..self.vols.count]) |r| {
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
        for (self.vols.rows[0..self.vols.count]) |r| {
            if (p.openPack(.{ .vol_tag = r.?.vol_tag, .scope = self.scope })) |op| if (op.base / self.chunk == block_idx) return true;
        }
        return false;
    }

    fn isInflight(ctx: ?*anyopaque, block_idx: u64) bool {
        const self: *DataPlane = @ptrCast(@alignCast(ctx.?));
        for (self.vols.rows[0..self.vols.count]) |r| if (r.?.alloc.inflightContains(block_idx)) return true;
        return false;
    }

    /// Every layout record in the xattr tree → its owner's bindings. The
    /// walk covers the NATIVE slot tree (local inos below the guest
    /// namespace base): a one-member forest owns no guest slot, and a
    /// range whose bounds route to two trees is refused by the backend.
    fn derivedWalk(self: *DataPlane, owners: *std.ArrayList(block_refs.OwnerBindings)) Error!void {
        const gpa = self.gpa;
        var cursor = key.KeyBuf.fromSlice(&key.xattrKey(0, 0, 0));
        const end = key.xattrKey(key.GUEST_NS_BASE - 1, key.HASH56_MAX, std.math.maxInt(u8));
        while (true) {
            var kvs: std.ArrayList(tree.KeyValue) = .empty;
            defer tree.deinitKeyValues(gpa, &kvs);
            self.backend.range(gpa, key.TREE_XATTRS, cursor.slice(), &end, block_refs.SCAN_PAGE, &kvs) catch |e| return backend_mod.KvBackend.coreError(e);
            if (kvs.items.len == 0) return;
            // The slot tree interleaves every ino-major kind: only the
            // xattr records are decoded; the cursor resumes at the next
            // xattr key after the page's last key, whatever its kind.
            for (kvs.items) |kv| {
                if (kv.key.len != key.XATTR_KEY_LEN or kv.key[key.INO_MAJOR_KIND_OFF] != key.TREE_XATTRS) continue;
                const xv = record.XattrValue.decode(kv.value) catch continue;
                if (!std.mem.eql(u8, xv.name, layout.LAYOUT_XATTR_NAME)) continue;
                const l = layout.Layout.decode(xv.value, self.geo) catch continue;
                const ino = std.mem.readInt(u64, kv.key[0..8], .big);
                var binds: std.ArrayList(block_refs.Binding) = .empty;
                var it = l.named(self.chunk);
                while (it.next()) |e| try binds.append(gpa, .{ .block_index = e.block, .vol_tag = e.mapping.vol_tag, .block_idx = e.mapping.blockIdx(self.chunk) });
                try owners.append(gpa, .{ .ino = ino, .bindings = try binds.toOwnedSlice(gpa) });
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
                if (last_ino >= key.GUEST_NS_BASE - 1) return;
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
        for (self.vols.rows[0..self.vols.count], 0..) |r, i| {
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
    inline for (.{ "\"data_volumes\":", "\"layout_map_merges\":", "\"layout_kvmap_crossing_refused\":", "\"held_stores\":", "\"hold_gate_admits\":", "\"prefetch_fibers\":", "\"bindings_freed\":", "\"data_unresolved_volume\":" }) |k| {
        try testing.expect(std.mem.indexOf(u8, s, k) != null);
    }
}
