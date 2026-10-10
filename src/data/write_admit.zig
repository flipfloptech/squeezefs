//! THE write admission (charter §6.2 "ONE write admission (the 'one path'
//! law): always the cheapest legal I/O … per page-aligned single-block
//! PIECE"; `docs/porting/data_write_path.md` §1, §8, §11, §14). The handler
//! owns the payload and level (1) (`op_guard.zig`); the data plane receives
//! the classification and runs, per piece under level (3), the ONE ordered
//! predicate list: A W1 patch → B device overlay → C one-authority screen →
//! D live existing-bytes re-derivation → E deferred-slot materialize →
//! F W2 extent park → G accumulation checkout → H/I sibling supersede → J
//! merge → K write-through → L park. Every exit drops the guard; exactly one
//! arm acks every piece.
//!
//! The rewrite-epoch arm gate (2026-10-07): a block a rewrite epoch BINDS —
//! open, or with its ONE save in flight (`Epochs.bindsBlock`) — is neither
//! W1-eligible (A would patch the A key the save is about to displace; the
//! patch vanishes under the swap) nor overlay-eligible (B would capture that
//! A key as its old binding and find it dead at the settle); it takes the
//! accumulation arms, whose base (D) is the write-side authority's key —
//! the epoch's newest shadow, else the map (`Epochs.baseBinding`) — so the
//! park's complement is seeded from the bytes B holds and its upload FEEDS
//! the epoch, displacing exactly that key. A base snapshotted from the
//! durable map under an open epoch lost the bytes only the shadow held.
//!
//! The write splitter makes pieces single-block: the kernel's writeback
//! grid straddles 4 MiB boundaries once a chunk end leaves the 1 MiB grid,
//! so PIECES are judged, never the whole write (§4.4ci). The ENOSPC
//! terminal law runs BEFORE any byte is recorded, decided for the WHOLE
//! write (`enospc.admitWrite`). `last_write_end` is a striped word table
//! keyed by `splitmix64(ino)` at the op-guard width (Q9: never an unbounded
//! per-ino map; a collision costs one misrouted adjacency verdict — the
//! accumulation path, never corruption), swapped UNCONDITIONALLY so
//! predicate 6 sees the true previous end even with the patch lever off.
//!
//! The admission ledger (Q1): ONE family `admit_write_{overlay,inplace,
//! accumulate,inline}_bytes` incremented at each arm's ACK site with the
//! identity `Σ ≡ user bytes at ACK` (the closure test); the per-mechanism
//! engagement counters stay as the arms' internal faces. Every knob row is
//! read ONCE at mount into `Config` (Q2: no per-module tri-state cell, no
//! second parser, no `cfg(test)` default); A/B levers log when non-default.

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const Atomic = std.atomic.Value;
const Allocator = std.mem.Allocator;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const log = @import("../core/log.zig");
const knobs = @import("../core/knobs.zig");
const derive = @import("../core/derive.zig");
const dlm = @import("../meta/dlm.zig");
const op_guard = @import("../fuse/handler/op_guard.zig");
const taskset = @import("../sync/taskset.zig");
const sync_tick = @import("../sync/tick.zig");
const lock_order = @import("../sync/lock_order.zig");
const budget = @import("../mem/budget.zig");
const layout = @import("layout.zig");
const staging = @import("staging.zig");
const block_alloc = @import("block_alloc.zig");
const enospc = @import("enospc.zig");
const router = @import("router.zig");
const seams = @import("write_seams.zig");
const cov = @import("write_coverage.zig");
const epoch_mod = @import("write_epoch.zig");
const pipeline = @import("write_pipeline.zig");
const device_overlay = @import("device_overlay.zig");
const patch_w1 = @import("patch_w1.zig");
const accumulate = @import("accumulate.zig");
const extent = @import("extent_overlay.zig");
const writeback = @import("writeback.zig");
const fsync_mod = @import("fsync.zig");
const write_side_view = @import("write_side_view.zig");

pub const Binding = seams.Binding;
pub const PAGE: u32 = seams.PAGE;

// ---------------------------------------------------------------------------
// Config: every row read once at mount.
// ---------------------------------------------------------------------------

pub const Config = struct {
    block_size: u64,
    patch_max: u64,
    overlay: device_overlay.Config,
    rewrite_shadow: bool,
    fold_max_extents: u64,
    fold_max_bytes: u64,
    parked_cap: u64,
    depth_override: pipeline.DepthOverride,
    fsync: fsync_mod.Levers,

    pub fn fromEnv(env: *const std.process.Environ.Map, block_size: u64, budget_bytes: u64) Config {
        const patch_max = patch_w1.resolvePatchMax(knobs.optIntKnob("SQUEEZEFS_PATCH_MAX_BYTES", u64, env), block_size);
        const cfg: Config = .{
            .block_size = block_size,
            .patch_max = patch_max,
            .overlay = .{
                .block_size = block_size,
                .enabled = knobs.boolKnob("SQUEEZEFS_DEVICE_OVERLAY", env),
                .overwrite = knobs.boolKnob("SQUEEZEFS_OVERLAY_OVERWRITE", env),
                .patch_max = patch_max,
                .gap_seed_ranged = knobs.boolKnob("SQUEEZEFS_GAP_SEED_RANGED", env),
                .close_barrier = knobs.boolKnob("SQUEEZEFS_OVERLAY_CLOSE_BARRIER", env),
                .depth_governor = knobs.boolKnob("SQUEEZEFS_OVERLAY_DEPTH_GOVERNOR", env),
                .ack_early = knobs.boolKnob("SQUEEZEFS_ZC_ACK_EARLY", env),
                .ack_early_odirect = knobs.boolKnob("SQUEEZEFS_ZC_ACK_EARLY_ODIRECT", env),
            },
            .rewrite_shadow = knobs.boolKnob("SQUEEZEFS_REWRITE_SHADOW", env),
            .fold_max_extents = extent.foldMaxExtentsFromEnv(env, block_size),
            .fold_max_bytes = extent.foldMaxBytesFromEnv(env, block_size),
            .parked_cap = accumulate.parkedCapFromEnv(env, budget_bytes, block_size),
            .depth_override = pipeline.depthOverrideFromEnv(env),
            .fsync = fsync_mod.Levers.fromEnv(env),
        };
        cfg.logNonDefault(block_size);
        return cfg;
    }

    /// A/B levers announce themselves when off their shipped arm.
    fn logNonDefault(self: Config, block_size: u64) void {
        if (self.patch_max != patch_w1.patchMaxDerived(block_size)) log.info("write path: A/B lever SQUEEZEFS_PATCH_MAX_BYTES={d} (derived {d})", .{ self.patch_max, patch_w1.patchMaxDerived(block_size) });
        if (!self.overlay.enabled) log.info("write path: A/B lever SQUEEZEFS_DEVICE_OVERLAY=0 (the accumulation control arm)", .{});
        if (!self.overlay.overwrite) log.info("write path: A/B lever SQUEEZEFS_OVERLAY_OVERWRITE=0", .{});
        if (self.overlay.close_barrier) log.info("write path: A/B lever SQUEEZEFS_OVERLAY_CLOSE_BARRIER=1 (the OW-8 posture)", .{});
        if (!self.overlay.depth_governor) log.info("write path: A/B lever SQUEEZEFS_OVERLAY_DEPTH_GOVERNOR=0", .{});
        if (!self.overlay.gap_seed_ranged) log.info("write path: A/B lever SQUEEZEFS_GAP_SEED_RANGED=0", .{});
        if (!self.overlay.ack_early) log.info("write path: A/B lever SQUEEZEFS_ZC_ACK_EARLY=0", .{});
        if (self.overlay.ack_early_odirect) log.info("write path: SQUEEZEFS_ZC_ACK_EARLY_ODIRECT=1 (the held-GUP snapshot opt-in)", .{});
        if (!self.rewrite_shadow) log.info("write path: A/B lever SQUEEZEFS_REWRITE_SHADOW=0 (per-block durable publishes)", .{});
        switch (self.depth_override) {
            .adaptive => {},
            .sync_inline => log.info("write path: A/B lever SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS=0 (synchronous inline uploads)", .{}),
            .pinned => |n| log.info("write path: A/B lever SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS={d} (pinned depth)", .{n}),
        }
        if (!self.fsync.touched_namespaces) log.info("write path: A/B lever SQUEEZEFS_FSYNC_TOUCHED_NAMESPACES=0 (every namespace barriers)", .{});
        if (!self.fsync.parallel_legs) log.info("write path: A/B lever SQUEEZEFS_FSYNC_PARALLEL_LEGS=0", .{});
    }
};

// ---------------------------------------------------------------------------
// Stats: the admission ledger + the prelude's counters.
// ---------------------------------------------------------------------------

pub const stats = struct {
    /// The charter's ledger: `Σ ≡ user bytes at ACK`.
    pub var admit_write_overlay_bytes: counters.ShardedCounter = .{};
    pub var admit_write_inplace_bytes: counters.ShardedCounter = .{};
    pub var admit_write_accumulate_bytes: counters.ShardedCounter = .{};
    pub var admit_write_inline_bytes: counters.ShardedCounter = .{};
    pub var write_enospc_refusals: counters.ShardedCounter = .{};
    pub var write_pieces: counters.ShardedCounter = .{};
    pub var writes_admitted: counters.ShardedCounter = .{};
    pub var staging_sibling_probes: counters.ShardedCounter = .{};
    pub var restage_churn_removes: counters.ShardedCounter = .{};
    pub var restage_churn_bytes: counters.ShardedCounter = .{};
    /// `parked_gate_waits`: visits to the parked-buffer gate at the pool
    /// cap (each a wait for a settle or a cold flush, never a refusal).
    pub var parked_gate_waits: counters.ShardedCounter = .{};
    /// `parked_gate_self_flushes`: the fold at the last moment (this
    /// writer's own block escalated, seeded and uploaded).
    pub var parked_gate_self_flushes: counters.ShardedCounter = .{};
    /// `parked_gate_cold_flushes`: a writer at the cap flushed the coldest
    /// parked block (untouched ≥ `PARKED_COLD_NS`) to make its cell.
    pub var parked_gate_cold_flushes: counters.ShardedCounter = .{};
    /// `parked_gate_wall_refusals`: writes answered ENOSPC after
    /// `PARKED_GATE_WALL_NS` of plane-refused cold flushes with no capacity
    /// returned (the plane is broken, not slow). Must stay 0 on a healthy
    /// set.
    pub var parked_gate_wall_refusals: counters.ShardedCounter = .{};
    pub var write_shared_class_violation: counters.Tripwire = .init("write_shared_class_violation");

    pub fn ledgerSum() u64 {
        return admit_write_overlay_bytes.load() + admit_write_inplace_bytes.load() + admit_write_accumulate_bytes.load() + admit_write_inline_bytes.load();
    }
};

/// The router's inline/staged arm acks through here (the ledger's fourth
/// family; the KV layout record IS the payload).
pub fn noteInlineWrite(len: u64) void {
    stats.admit_write_inline_bytes.add(len);
}

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.writeAll("{");
    inline for (.{ "admit_write_overlay_bytes", "admit_write_inplace_bytes", "admit_write_accumulate_bytes", "admit_write_inline_bytes", "write_enospc_refusals", "write_pieces", "writes_admitted", "staging_sibling_probes", "restage_churn_removes", "restage_churn_bytes", "parked_gate_waits", "parked_gate_self_flushes", "parked_gate_cold_flushes", "parked_gate_wall_refusals" }) |name| {
        try w.print("\"{s}\":{d},", .{ name, @field(stats, name).load() });
    }
    try w.print("\"write_shared_class_violation\":{d},", .{counters.tripwireCount("write_shared_class_violation")});
    try w.writeAll("\"overlay\":");
    try device_overlay.writeStatsJson(w);
    try w.writeAll(",\"patch\":");
    try patch_w1.writeStatsJson(w);
    try w.writeAll(",\"accumulate\":");
    try accumulate.writeStatsJson(w);
    try w.writeAll(",\"extent\":");
    try extent.writeStatsJson(w);
    try w.writeAll(",\"rewrite_epoch\":");
    try epoch_mod.writeStatsJson(w);
    try w.writeAll(",\"pipeline\":");
    try pipeline.writeStatsJson(w);
    try w.writeAll(",\"writeback\":");
    try writeback.writeStatsJson(w);
    try w.writeAll(",\"fsync\":");
    try fsync_mod.writeStatsJson(w);
    try w.writeAll(",\"write_side_view\":");
    try write_side_view.writeStatsJson(w);
    try w.writeAll(",\"enospc\":");
    try enospc.writeStatsJson(w);
    try w.writeAll("}");
}

// ---------------------------------------------------------------------------
// The splitter and the per-ino last-write-end word.
// ---------------------------------------------------------------------------

pub const Piece = struct {
    block: u32,
    /// Byte offset within the block.
    rel: u32,
    /// Byte offset within the payload.
    off: u64,
    len: u32,
};

/// Cut `[offset, offset + len)` at block boundaries: every piece is
/// single-block; `out.len ≥ ceil(len / bs) + 1`.
pub fn splitPieces(offset: u64, len: u64, bs: u64, out: []Piece) []Piece {
    var n: usize = 0;
    var pos: u64 = 0;
    while (pos < len) {
        const abs = offset + pos;
        const block: u32 = @intCast(abs / bs);
        const rel: u32 = @intCast(abs % bs);
        const take: u32 = @intCast(@min(bs - rel, len - pos));
        out[n] = .{ .block = block, .rel = rel, .off = pos, .len = take };
        n += 1;
        pos += take;
    }
    return out[0..n];
}

/// `max_pieces(len, bs) = len / bs + 2` (a straddling write of `len` bytes
/// touches at most that many blocks).
pub fn maxPieces(len: u64, bs: u64) usize {
    return @intCast(len / bs + 2);
}

pub const LastWriteEnd = struct {
    words: []Atomic(u64),
    mask: u64,

    /// 0 = "no previous write" (an offset-0 write is never adjacent to nothing).
    pub fn init(gpa: Allocator, stripes: usize) Allocator.Error!LastWriteEnd {
        assert(std.math.isPowerOfTwo(stripes));
        const words = try gpa.alloc(Atomic(u64), stripes);
        @memset(words, .init(0));
        return .{ .words = words, .mask = stripes - 1 };
    }

    pub fn deinit(self: *LastWriteEnd, gpa: Allocator) void {
        gpa.free(self.words);
        self.* = undefined;
    }

    /// Swap UNCONDITIONALLY; answers the previous end (0 = none).
    pub fn note(self: *LastWriteEnd, ino: u64, offset: u64, len: u64) u64 {
        return self.words[@intCast(dlm.splitmix64(ino) & self.mask)].swap(offset + len, .monotonic);
    }

    /// The word as it stands (the W1 hold gate's adjacency read on the
    /// queue thread — one relaxed load, no swap).
    pub fn peek(self: *const LastWriteEnd, ino: u64) u64 {
        return self.words[@intCast(dlm.splitmix64(ino) & self.mask)].load(.monotonic);
    }
};

// ---------------------------------------------------------------------------
// The write path.
// ---------------------------------------------------------------------------

/// What an adoption hands the merge (charter §7 "placed sever … the first
/// merge ADOPTS"): the assembly's cell — the registry's share now the
/// caller's — and the LANDED runs the entry records as coverage.
pub const Adoption = struct {
    cell: *accumulate.Pool.Cell,
    n_runs: usize,
};

/// The placed-sever registry's face on the write path (the il host binds
/// it; `ipc/host/placed.zig` is the registry). Null = no registry: a
/// placed payload merges by copy like a pooled one.
pub const PlacedSeam = struct {
    pub const max_runs = cov.CoverageUnion.max_runs;

    ctx: *anyopaque,
    /// Entry-absent, under (3): the assembly of `(ino, block)` whose region
    /// at `rel` IS `ptr` (the pointer proof), sealed for adoption; its
    /// landed runs into `runs`. Null = no adoption (the copy path).
    take: *const fn (ctx: *anyopaque, ino: u64, block: u32, rel: u32, ptr: [*]const u8, runs: *[cov.CoverageUnion.max_runs]cov.Run) ?Adoption,
    /// Did this payload's assembly get adopted (its bytes are already the
    /// entry's coverage — the merge is a no-op ack)?
    adopted: *const fn (ctx: *anyopaque, ino: u64, block: u32, token: u64) bool,
};

/// A placed-sever payload: bytes already living in a shared assembly's
/// cell (`cell.data[rel..][0..len]`), the token naming its assembly life.
pub const PlacedPayload = struct {
    cell: *accumulate.Pool.Cell,
    rel: u32,
    len: u32,
    token: u64,
    seam: PlacedSeam,

    pub fn bytes(self: PlacedPayload) []const u8 {
        return self.cell.data[self.rel .. self.rel + self.len];
    }
};

/// The owned payload (the handler's lease by value): bytes in hand, a
/// HELD zero-copy slot (W1/overlay direct-leg candidates; every other
/// shape materializes once), or a PLACED sever (bytes in an assembly the
/// merge adopts or copies from).
pub const Payload = union(enum) {
    bytes: []const u8,
    slot: device_overlay.SlotVehicle,
    placed: PlacedPayload,

    pub fn len(self: Payload) u64 {
        return switch (self) {
            .bytes => |b| b.len,
            .slot => |s| s.src.len,
            .placed => |p| p.len,
        };
    }
};

pub const Request = struct {
    ino: u64,
    offset: u64,
    payload: Payload,
    /// The handler's level-1 classification (the shared-mode handoff).
    class: op_guard.Class,
    /// The layout class from the caches (a miss ⇒ `null`: the prelude
    /// treats it as "no map resident").
    file: ?seams.FileView,
    /// The op's watchdog station for the parked-pool gate (null: no op
    /// behind this write — a truncate's zero fill, the tests).
    park_hook: ?ParkHook = null,
};

/// The gate's station seam: `parked` runs once per gate visit, before the
/// wait, on the writer's own fiber (the FUSE handler stamps
/// `write_parked_pool` so the op watchdog names the park).
pub const ParkHook = struct {
    ctx: *anyopaque,
    parked: *const fn (ctx: *anyopaque) void,
};

/// A parked block untouched for a whole liveness tick is COLD: the pieces
/// that would complete it are not in flight (a hot block under a QD32
/// stream sees its siblings within milliseconds). At the pool cap a cold
/// block is flushed by the writer that needs its cell — one bounded upload
/// — while a hot set with an upload in flight is waited on (that upload
/// returns a cell at the device's pace); a set with NO upload in flight is
/// flushed at once whatever its age, since nothing else can return a cell
/// and the writer holds its inode guard meanwhile. The tick is
/// `sync/tick.zig`'s 2 s: the one liveness period every unbounded wait
/// rides, so a write parked here is never quiet longer than any other park.
pub const PARKED_COLD_NS: u64 = sync_tick.tick_ns;

/// How long the gate keeps retrying cold flushes the plane REFUSES (a
/// terminal `StorageFull` after the valve drained, a failed DMA) with no
/// capacity returned before the write answers ENOSPC: the allocation
/// layer's own wall (`enospc.PRESSURE_PARK_WALL_FLOOR_MS`, 1 s — "past it
/// the refusal stands, loud: the plane is broken, not slow"). The gate
/// retries once per tick, so the second refused flush crosses it: a write
/// occupies its inode guard for two ticks at most on a starving store,
/// not the 30 s that queued a QD32 stream behind one parked writer past
/// the harness's bound (rung 9, the direct row on the 512m aging store).
pub const PARKED_GATE_WALL_NS: u64 = enospc.PRESSURE_PARK_WALL_FLOOR_MS * std.time.ns_per_ms;

pub const Arm = enum(u8) { patch, overlay, extent, accumulate_park, write_through };

pub const Outcome = struct {
    written: u64,
    /// The size the write's bytes establish (the postlude's size claim).
    size_claim: u64,
    pieces: u32,
    arms: [5]u32 = @splat(0),

    pub fn count(self: *const Outcome, a: Arm) u32 {
        return self.arms[@backingInt(a)];
    }
};

pub const Error = error{ NoSpaceLeft, InputOutput, VolumeFailed, WriterGuardFenced, OutOfMemory, WorkerShutdown, QueueFull, ReadOnlyFileSystem, FileTooBig };

/// The data volumes' allocators as a picker: STRIPE-UNIT placement
/// (`router.stripeUnitChoice`) over the active volumes with supply — a
/// file's consecutive blocks share one volume for a whole unit so the
/// block-map merge emits runs; a volume whose fresh supply latched leaves
/// the eligible set (the unit's choice re-derives over the survivors).
pub const Picker = struct {
    vols: *const seams.Volumes,
    /// The overlay's and W1's shape: the stored image IS the payload, so
    /// their destinations are passthrough volumes only.
    passthrough_only: bool = false,

    pub fn picker(self: *Picker) enospc.Picker {
        return .{ .ctx = self, .pick = &pick };
    }

    fn eligible(self: *const Picker, v: *const seams.VolumeInfo) bool {
        return v.isActive() and (!self.passthrough_only or v.passthrough);
    }

    fn pick(ctx: ?*anyopaque, place: enospc.Place) ?*block_alloc.BlockAllocator {
        const self: *Picker = @ptrCast(@alignCast(ctx.?));
        var with_supply: [router.MAX_VOLUMES]*block_alloc.BlockAllocator = undefined;
        var n: u32 = 0;
        var fallback: ?*block_alloc.BlockAllocator = null;
        for (self.vols.rows[0..self.vols.n()]) |*r| {
            const v = &r.*.?;
            if (!self.eligible(v)) continue;
            if (fallback == null) fallback = v.alloc;
            if (v.alloc.freshSupplyLatched()) continue;
            with_supply[n] = v.alloc;
            n += 1;
        }
        if (n == 0) return fallback;
        return with_supply[router.stripeUnitChoice(place.ino, place.block, n)];
    }
};

pub const WritePath = struct {
    gpa: Allocator,
    io: Io,
    cfg: Config,
    vols: *const seams.Volumes,
    device: seams.DeviceSeam,
    map: seams.MapSeam,
    free: seams.FreeSeam,
    purge: seams.PurgeSeam,
    picker: Picker,
    overlay_picker: Picker,
    valve: ?enospc.Valve,
    epochs: epoch_mod.Epochs,
    stripes: *layout.MetaStripes,
    locks: seams.BlockLocks,
    governor: pipeline.Governor,
    overlay: device_overlay.Overlay,
    table: accumulate.Table,
    touched: fsync_mod.TouchedTable,
    last_write_end: LastWriteEnd,
    view: write_side_view.Composite,
    tasks: taskset.TaskSet = .init,
    transform: accumulate.TransformSeam,
    scratch: ?[]align(PAGE) u8,
    /// Some active volume is passthrough (the overlay has a destination).
    any_passthrough: bool,

    pub const Deps = struct {
        /// The mount's `Io` for the slow paths that free parked supply
        /// (the pressure hook); hot DMA submission never takes it.
        io: Io,
        vols: *const seams.Volumes,
        device: seams.DeviceSeam,
        map: seams.MapSeam,
        free: seams.FreeSeam,
        purge: seams.PurgeSeam = .none,
        valve: ?enospc.Valve = null,
        stripes: *layout.MetaStripes,
        mem: ?*const budget.MemBudget = null,
        budget_bytes: u64,
        transform: accumulate.TransformSeam = .passthrough,
        /// `transform.scratchLen(block)` when any volume is transformed; 0 otherwise.
        scratch_len: usize = 0,
    };

    pub fn init(gpa: Allocator, cfg: Config, d: Deps) Allocator.Error!*WritePath {
        const self = try gpa.create(WritePath);
        errdefer gpa.destroy(self);
        var passthrough = false;
        for (d.vols.rows[0..d.vols.n()]) |*r| if (r.*.?.passthrough and r.*.?.isActive()) {
            passthrough = true;
        };
        self.* = .{
            .gpa = gpa,
            .io = d.io,
            .cfg = cfg,
            .vols = d.vols,
            .device = d.device,
            .map = d.map,
            .free = d.free,
            .purge = d.purge,
            .picker = .{ .vols = d.vols },
            .overlay_picker = .{ .vols = d.vols, .passthrough_only = true },
            .valve = d.valve,
            .epochs = epoch_mod.Epochs.init(gpa, cfg.block_size, cfg.rewrite_shadow),
            .stripes = d.stripes,
            .locks = undefined,
            .governor = undefined,
            .overlay = undefined,
            .table = undefined,
            .touched = undefined,
            .last_write_end = undefined,
            .view = undefined,
            .transform = d.transform,
            .scratch = null,
            .any_passthrough = passthrough,
        };
        errdefer self.epochs.deinit();
        self.locks = try seams.BlockLocks.init(gpa, seams.BLOCK_LOCK_STRIPES);
        errdefer self.locks.deinit(gpa);
        self.governor = try pipeline.Governor.init(gpa, cfg.block_size, @max(d.vols.n(), 1), cfg.depth_override, d.mem, pipeline.capBytes(d.budget_bytes));
        errdefer self.governor.deinit();
        self.table = try accumulate.Table.init(gpa, @intCast(cfg.block_size), cfg.parked_cap);
        errdefer self.table.deinit();
        self.touched = try fsync_mod.TouchedTable.init(gpa, op_guard.INODE_GUARD_STRIPES);
        errdefer self.touched.deinit(gpa);
        self.last_write_end = try LastWriteEnd.init(gpa, op_guard.INODE_GUARD_STRIPES);
        errdefer self.last_write_end.deinit(gpa);
        if (d.scratch_len != 0) self.scratch = try gpa.alignedAlloc(u8, .fromByteUnits(PAGE), d.scratch_len);
        self.overlay = try device_overlay.Overlay.init(gpa, cfg.overlay, .{
            .device = d.device,
            .map = d.map,
            .free = d.free,
            .purge = d.purge,
            .vols = d.vols,
            .picker = self.overlay_picker.picker(),
            .valve = d.valve,
            .pressure_hook = .{ .ctx = self, .run = &pressureHook },
            .epochs = &self.epochs,
            .stripes = d.stripes,
            .governor = &self.governor,
        }, @intCast(cfg.parked_cap));
        self.view = .{ .table = &self.table, .overlay = &self.overlay, .epochs = &self.epochs, .locks = &self.locks };
        return self;
    }

    pub fn deinit(self: *WritePath, io: Io) void {
        self.tasks.deinit(io);
        self.overlay.deinit(io);
        if (self.scratch) |s| self.gpa.free(s);
        self.last_write_end.deinit(self.gpa);
        self.touched.deinit(self.gpa);
        self.table.deinit();
        self.governor.deinit();
        self.locks.deinit(self.gpa);
        self.epochs.deinit();
        self.gpa.destroy(self);
    }

    /// The read path's seam.
    pub fn writeSideView(self: *WritePath) write_side_view.WriteSideView {
        return self.view.view();
    }

    pub fn closeSeams(self: *WritePath) epoch_mod.Epochs.CloseSeams {
        return .{ .map = self.map, .free = self.free, .purge = self.purge };
    }

    fn pressureHook(ctx: ?*anyopaque) bool {
        const self: *WritePath = @ptrCast(@alignCast(ctx.?));
        return self.epochs.closeAllForSpace(self.io, self.closeSeams());
    }

    pub fn uploadDeps(self: *WritePath) accumulate.UploadDeps {
        return .{ .device = self.device, .map = self.map, .free = self.free, .purge = self.purge, .vols = self.vols, .picker = self.picker.picker(), .valve = self.valve, .epochs = &self.epochs, .stripes = self.stripes, .locks = &self.locks, .transform = self.transform, .scratch = self.scratch, .touched = &touchedStamp, .touched_ctx = self };
    }

    fn touchedStamp(ctx: ?*anyopaque, ino: u64, ordinal: u32) void {
        const self: *WritePath = @ptrCast(@alignCast(ctx.?));
        self.touched.stampOrdinal(ino, ordinal);
    }

    pub fn patchDeps(self: *WritePath) patch_w1.Deps {
        return .{ .device = self.device, .map = self.map, .vols = self.vols, .purge = self.purge, .custody = .{ .ctx = self, .ram_custody = &ramCustody, .overlay_open = &overlayOpen }, .touched = .{ .ctx = self, .stamp = &touchedStamp } };
    }

    // ---- the il host's faces (charter §7) ----

    /// Placed-sever eligibility (`il_daemon.md` §3.2, the six screens plus
    /// the port's arm screen), cheapest first: a page-multiple block; `rel`
    /// and `len` page-aligned inside one block; `len` above the W1 cap (a
    /// raised cap widens patch and narrows placement together) AND at
    /// least the accumulation class (below a quarter block the W2 extent
    /// park is the cheaper legal arm and copies anyway); the EFBIG mirror;
    /// a resident striped layout; no live RAM entry, overlay record or
    /// epoch binding for the block (an entry-present block never adopts);
    /// and the device overlay NOT eligible for the shape (it would take the
    /// piece with zero copies first — a placement there is never adopted).
    pub fn placedSeverEligible(self: *WritePath, ino: u64, offset: u64, len: u64, max_file_size: u64) bool {
        const bs = self.cfg.block_size;
        if (bs % PAGE != 0 or len == 0) return false;
        const rel = offset % bs;
        if (rel % PAGE != 0 or len % PAGE != 0 or rel + len > bs) return false;
        if (len <= self.cfg.patch_max or extent.isSmall(len, bs)) return false;
        if (offset + len > max_file_size) return false;
        const file = self.map.view(ino) orelse return false;
        if (file.kind != .striped or !file.map_resident) return false;
        const block: u32 = @intCast(offset / bs);
        if (self.table.has(ino, block) or self.overlay.hasCustody(ino, block) or self.epochs.bindsBlock(ino, block)) return false;
        return !self.overlay.shapeEligible(len, self.any_passthrough);
    }

    /// Why the direct-drive write gate declined (each its own counter).
    pub const DirectDecline = enum { shape, block_lock, custody };

    /// A W1-shaped write begun on the direct-drive lane: the held (3)
    /// guard and the fence-passed patch; the lane DMAs ONE SQE at
    /// `begun.dest` and calls `finishDirectWrite` at the CQE.
    pub const DirectBegun = struct {
        guard: seams.BlockLocks.Guard,
        begun: patch_w1.Begun,
        len: u64,
    };

    pub const DirectVerdict = union(enum) {
        begun: DirectBegun,
        declined: DirectDecline,
    };

    /// The il direct-drive write gate (`il_daemon.md` §4.5): the lock-free
    /// shape half (`patch_w1.shapeEligible` with the per-ino word READ, not
    /// swapped — a declined candidate must not disturb adjacency), then ONE
    /// `BlockLocks.tryAcquire` — never a blocking acquire on the service
    /// thread — then the state half under the held guard (`beginPatch`:
    /// custody, the authoritative map, the sole-owner verdict, the fence
    /// pair). On `.begun` the guard is HELD across the SQE (level 3 only;
    /// nothing is acquired under it) and the per-ino word is COMMITTED
    /// (`last_write_end`): the coverage bookkeeping for this shape.
    pub fn beginDirectWrite(self: *WritePath, io: Io, ino: u64, offset: u64, len: u64) PatchError!DirectVerdict {
        const bs = self.cfg.block_size;
        const file = self.map.view(ino) orelse return .{ .declined = .shape };
        if (file.kind != .striped or !file.map_resident) return .{ .declined = .shape };
        const prev_end = self.last_write_end.peek(ino);
        if (!patch_w1.shapeEligible(.{ .offset = offset, .len = len, .block_size = bs, .existing_size = file.size, .stream_adjacent = prev_end != 0 and prev_end == offset, .patch_max = self.cfg.patch_max })) return .{ .declined = .shape };
        const block: u32 = @intCast(offset / bs);
        const rel: u32 = @intCast(offset % bs);
        const guard = self.locks.tryAcquire(io, ino, block) orelse return .{ .declined = .block_lock };
        const begun = (patch_w1.beginPatch(self.patchDeps(), ino, block, rel) catch |e| {
            guard.release(io);
            return e;
        }) orelse {
            guard.release(io);
            return .{ .declined = .custody };
        };
        _ = self.last_write_end.note(ino, offset, len);
        // The in-flight SQE owns the guard from here: its release runs at
        // the CQE on whichever thread reaps (the owner's inline reap or the
        // shard's reaper), so the acquiring context's ledger entry leaves
        // now — a stale (3) would read every later op guard on this thread
        // as an inversion.
        lock_order.handedOff(.block);
        return .{ .begun = .{ .guard = guard, .begun = begun, .len = len } };
    }

    pub const PatchError = patch_w1.PatchError;

    /// A begun direct write the lane could not submit (no slab entry, a
    /// full SQ, an unknown volume): re-stabilize the word under a new
    /// generation (content unchanged, so no purge), hand the guard back;
    /// the caller takes the sever → handoff path.
    pub fn abortDirectWrite(self: *WritePath, io: Io, d: DirectBegun) void {
        if (self.vols.byTag(d.begun.vol_tag)) |vol| vol.alloc.publishBlock(d.begun.idx);
        d.guard.release(io);
    }

    /// The CQE postlude: `finishPatch` (re-stabilize, purge, the touched
    /// stamp, the ledger — or the EIO law), then the guard drops.
    pub fn finishDirectWrite(self: *WritePath, io: Io, d: DirectBegun, dma: seams.DmaError!void) PatchError!void {
        defer d.guard.release(io);
        try patch_w1.finishPatch(self.patchDeps(), d.begun, d.len, dma);
        stats.admit_write_inplace_bytes.add(d.len);
        stats.writes_admitted.inc();
    }

    fn ramCustody(ctx: ?*anyopaque, ino: u64, block: u32) bool {
        const self: *WritePath = @ptrCast(@alignCast(ctx.?));
        return self.table.has(ino, block) or self.epochs.bindsBlock(ino, block);
    }

    fn overlayOpen(ctx: ?*anyopaque, ino: u64, block: u32) bool {
        const self: *WritePath = @ptrCast(@alignCast(ctx.?));
        return self.overlay.hasCustody(ino, block);
    }

    pub fn fsyncCtx(self: *WritePath) fsync_mod.Ctx {
        return .{
            .levers = self.cfg.fsync,
            .device = self.device,
            .vols = self.vols,
            .touched = &self.touched,
            .locks = &self.locks,
            .overlay = &self.overlay,
            .table = &self.table,
            .upload = self.uploadDeps(),
            .epochs = &self.epochs,
            .close_seams = self.closeSeams(),
            .meta_sync = .{ .ctx = null, .sync = &noMetaSync },
        };
    }

    fn noMetaSync(_: ?*anyopaque, _: Io, _: u64) anyerror!void {}

    // ---- §8 the custody probe ----

    /// RAM-only: a block is HELD iff custody exists (parked, overlay, epoch)
    /// or the write can land IN PLACE (passthrough ∧ undecorated whole-block
    /// ∧ active volume ∧ `refcount == 1 ∧ !SHARED`); a hole below EOF holds
    /// nothing; a cache miss answers "held" (the refusal must never fire on
    /// a block whose mapping it could not see).
    pub fn writeWouldCreateFreshCustody(self: *WritePath, ino: u64, file: ?seams.FileView, first: u32, last: u32) bool {
        const f = file orelse return false;
        if (!f.map_resident) return false;
        var b = first;
        while (b <= last) : (b += 1) {
            if (self.table.has(ino, b) or self.overlay.hasCustody(ino, b) or self.epochs.bindsBlock(ino, b)) continue;
            const m = self.map.lookup(ino, b) orelse return true;
            const v = self.vols.byTag(m.vol_tag) orelse return true;
            if (!v.passthrough or !m.whole_block or !v.isActive()) return true;
            const idx = v.alloc.idxOf(m.offset);
            if (v.alloc.refcount(idx) != 1 or v.alloc.isShared(idx)) return true;
        }
        return false;
    }

    // ---- the write ----

    /// The whole write: the prelude screens (the ENOSPC terminal law BEFORE
    /// any byte is recorded; the per-ino word; the W1 request-shape half;
    /// the overlay's write-wide shape), then one piece at a time through the
    /// ladder. `written == len` on success (every piece acked by exactly one
    /// arm); an error acks NOTHING of the failing piece and never loses a
    /// piece already acked.
    pub fn write(self: *WritePath, io: Io, req: Request) Error!Outcome {
        const len = req.payload.len();
        const bs = self.cfg.block_size;
        if (len == 0) return .{ .written = 0, .size_claim = 0, .pieces = 0 };
        const end = std.math.add(u64, req.offset, len) catch return error.FileTooBig;
        if (end / bs >= std.math.maxInt(u32)) return error.FileTooBig;
        const file = req.file orelse seams.FileView{ .kind = .striped, .size = 0, .map_resident = false };
        // The shared-mode handoff: a Shared classification is a cache-
        // resident striped write with `end ≤ size` — anything else here is
        // the handler's classifier disagreeing with the data plane.
        if (req.class == .shared and (file.kind != .striped or end > file.size)) {
            stats.write_shared_class_violation.fire("a Shared-classified write extends the file or is not striped");
        }
        const first: u32 = @intCast(req.offset / bs);
        const last: u32 = @intCast((end - 1) / bs);
        // §8: the ENOSPC terminal law, decided for the WHOLE write.
        var cands: [router.MAX_VOLUMES]*block_alloc.BlockAllocator = undefined;
        if (enospc.admitWrite(enospc.freshSupplyExhausted(self.vols.candidates(&cands)), self.writeWouldCreateFreshCustody(req.ino, req.file, first, last)) == .refuse_enospc) {
            stats.write_enospc_refusals.inc();
            return error.NoSpaceLeft;
        }
        // The per-ino word, swapped unconditionally.
        const prev_end = self.last_write_end.note(req.ino, req.offset, len);
        const stream_adjacent = prev_end != 0 and prev_end == req.offset;
        // The W1 request-shape half (one bucket per striped write).
        const try_patch = file.kind == .striped and file.map_resident and
            patch_w1.shapeEligible(.{ .offset = req.offset, .len = len, .block_size = bs, .existing_size = file.size, .stream_adjacent = stream_adjacent, .patch_max = self.cfg.patch_max });
        const overlay_shape = self.overlay.shapeEligible(len, self.any_passthrough) and file.kind == .striped and file.map_resident;
        var pieces_buf: [64]Piece = undefined;
        if (maxPieces(len, bs) > pieces_buf.len) return error.FileTooBig;
        const pieces = splitPieces(req.offset, len, bs, &pieces_buf);
        // Deferred slot: a held payload stays unmaterialized only for the
        // single-piece direct-leg candidates; every other shape materializes
        // HERE once.
        var bytes: ?[]const u8 = switch (req.payload) {
            .bytes => |b| b,
            .placed => |p| p.bytes(),
            .slot => null,
        };
        const single = pieces.len == 1;
        if (bytes == null and !(single and (try_patch or overlay_shape))) {
            bytes = self.materialize(io, req.payload) catch return error.InputOutput;
        }
        var out: Outcome = .{ .written = 0, .size_claim = end, .pieces = @intCast(pieces.len) };
        for (pieces) |pc| {
            stats.write_pieces.inc();
            const arm = try self.admitPiece(io, req, file, pc, &bytes, try_patch and single, overlay_shape, stream_adjacent);
            out.arms[@backingInt(arm)] += 1;
            out.written += pc.len;
        }
        stats.writes_admitted.inc();
        return out;
    }

    fn materialize(_: *WritePath, io: Io, p: Payload) ![]const u8 {
        return switch (p) {
            .bytes => |b| b,
            .placed => |pl| pl.bytes(),
            .slot => |s| (s.extract orelse return error.SlotNotHeld)(s.ctx, io),
        };
    }

    /// Under (3), THIS order (every exit drops the guard). The ladder
    /// RESTARTS from A when the one-authority screen (C) meets a record it
    /// cannot settle in one attempt (the guard is dropped, the record
    /// settled on the write arm's own ladder, the guard re-taken) and when
    /// an accumulation arm meets the parked-buffer cap (the guard is
    /// dropped, the gate waits for a cell or makes one, the guard re-taken)
    /// — never a backoff park under (3), never a refusal: a blocking
    /// `write(2)` on a regular file may not answer EAGAIN (POSIX reserves
    /// it for O_NONBLOCK; rung 9 F2 saw fio abort on 100–128 per leg).
    fn admitPiece(self: *WritePath, io: Io, req: Request, file: seams.FileView, pc: Piece, bytes: *?[]const u8, try_patch: bool, overlay_shape: bool, stream_adjacent: bool) Error!Arm {
        const bs = self.cfg.block_size;
        var hint: enum { none, overlay_drain, upload, fold } = .none;
        var upload_permit: ?pipeline.Permit = null;
        var prof = pipeline.Prof.start();
        var gate: GateWall = .{};
        const arm: Arm = blk: while (true) {
            const g = self.locks.acquire(io, req.ino, pc.block, .write_checkout);
            var held = true;
            defer if (held) g.release(io);
            prof.mark(.lock_wait);
            // A. W1.
            if (try_patch) {
                const vehicle: patch_w1.Vehicle = if (bytes.*) |b| .{ .bytes = b[pc.off .. pc.off + pc.len] } else .{ .slot = req.payload.slot.src };
                if (try patch_w1.tryPatch(self.patchDeps(), io, .{ .ino = req.ino, .block = pc.block, .rel = pc.rel, .vehicle = vehicle })) {
                    stats.admit_write_inplace_bytes.add(pc.len);
                    break :blk .patch;
                }
            }
            // B. Device overlay.
            if (overlay_shape and device_overlay.Overlay.pieceEligible(pc.rel, pc.len)) {
                const vehicle: device_overlay.Vehicle = if (bytes.*) |b| .{ .bytes = b[pc.off .. pc.off + pc.len] } else .{ .slot = req.payload.slot };
                const overwrite = self.map.lookup(req.ino, pc.block) != null;
                switch (try self.overlay.store(io, .{ .ino = req.ino, .block = pc.block, .rel = pc.rel, .vehicle = vehicle, .file = file, .overwrite = overwrite })) {
                    .stored => |s| {
                        stats.admit_write_overlay_bytes.add(pc.len);
                        if (s.coverage_complete) hint = .overlay_drain;
                        break :blk .overlay;
                    },
                    .declined => {},
                }
            }
            // C. The one-authority screen: a record on the block settles first.
            if (self.overlay.anyOpenFast()) {
                if (self.overlay.registry.get(.{ .ino = req.ino, .block = pc.block })) |rec| {
                    if (self.overlay.settleOnceLocked(io, rec, false) == .transient) {
                        held = false;
                        g.release(io);
                        _ = self.overlay.settleWriteArm(io, &self.locks, req.ino, pc.block, false);
                        continue;
                    }
                }
            }
            // D. Live existing-bytes re-derivation under the guard, from the
            //    write-side authority (module doc: the arm gate).
            const live_binding = self.epochs.baseBinding(self.map, req.ino, pc.block);
            const b_start = @as(u64, pc.block) * bs;
            const live_size = if (self.map.view(req.ino)) |v| v.size else file.size;
            const has_existing = live_binding != null or @min(live_size, b_start + bs) > b_start;
            // E. Deferred-slot materialize.
            if (bytes.* == null) bytes.* = self.materialize(io, req.payload) catch return error.InputOutput;
            const data = bytes.*.?[pc.off .. pc.off + pc.len];
            // F. W2 extent park.
            const small = extent.isSmall(pc.len, bs);
            const existing = self.table.get(req.ino, pc.block);
            if (small and !stream_adjacent and (existing == null or existing.?.repr == .extent)) {
                const e = existing orelse self.table.checkoutExtent(req.ino, pc.block, has_existing, live_binding) orelse {
                    try self.parkAtPoolCap(io, g, &held, req, pc.block, &gate);
                    continue;
                };
                e.repr.extent.merge(self.gpa, pc.rel, data) catch return error.OutOfMemory;
                const v = e.coverage.record(pc.rel, pc.rel + pc.len, @intCast(bs)) catch {
                    // The run table is full on an extent entry: escalate (the slabs lay into a buffer).
                    if (self.table.escalate(e) == null) {
                        try self.parkAtPoolCap(io, g, &held, req, pc.block, &gate);
                        continue;
                    }
                    extent.stats.extent_escalations.inc();
                    stats.admit_write_accumulate_bytes.add(pc.len);
                    break :blk .accumulate_park;
                };
                if (v.completed) extent.stats.extent_overlay_completed_coverage.fire("an extent overlay completed the block's coverage");
                e.write_epoch = accumulate.nextWriteEpoch();
                e.app_end = @max(e.app_end, pc.rel + pc.len);
                extent.stats.extent_parks.inc();
                // The piece is RECORDED either way; an escalation the pool
                // cannot back now is DEFERRED (the extent stays parked; the
                // block's next write or its flush escalates once a cell
                // exists) — never a refusal, never a park under (3).
                if (v.completed or extent.escalationDue(e.repr.extent.bytes, bs)) {
                    if (self.table.escalate(e) != null) extent.stats.extent_escalations.inc() else extent.stats.extent_escalations_deferred.inc();
                } else if (e.repr.extent.count() >= self.cfg.fold_max_extents or e.repr.extent.bytes >= self.cfg.fold_max_bytes) {
                    // The fold at the last moment: escalate, seed once, upload.
                    if (self.table.escalate(e) != null) {
                        extent.stats.extent_escalations.inc();
                        hint = .fold;
                    } else extent.stats.extent_escalations_deferred.inc();
                }
                stats.admit_write_accumulate_bytes.add(pc.len);
                break :blk .extent;
            }
            // G. Accumulation checkout (an extent entry escalates first). A
            //    PLACED payload whose assembly the first merge already
            //    adopted is a no-op ack (its landed bytes ARE that entry's
            //    coverage); entry-absent, a pointer-proven assembly is
            //    adopted as the backing with its landed runs recorded —
            //    a full union at adoption IS the write-through trigger.
            var e = existing orelse blk_g: {
                if (req.payload == .placed) {
                    const p = req.payload.placed;
                    if (p.seam.adopted(p.seam.ctx, req.ino, pc.block, p.token)) {
                        stats.admit_write_accumulate_bytes.add(pc.len);
                        break :blk .accumulate_park;
                    }
                    var runs: [cov.CoverageUnion.max_runs]cov.Run = undefined;
                    if (p.seam.take(p.seam.ctx, req.ino, pc.block, pc.rel, data.ptr, &runs)) |ad| {
                        const adopted = self.table.checkoutAdopting(req.ino, pc.block, has_existing, live_binding, ad.cell, runs[0..ad.n_runs]) orelse {
                            self.table.pool.release(ad.cell);
                            try self.parkAtPoolCap(io, g, &held, req, pc.block, &gate);
                            continue;
                        };
                        stats.admit_write_accumulate_bytes.add(pc.len);
                        if (adopted.coverage.isFull(@intCast(bs))) {
                            hint = .upload;
                            break :blk .write_through;
                        }
                        break :blk .accumulate_park;
                    }
                }
                break :blk_g self.table.checkout(req.ino, pc.block, has_existing, live_binding) orelse {
                    // The pool is at its cap: the gate parks, never a lost byte.
                    try self.parkAtPoolCap(io, g, &held, req, pc.block, &gate);
                    continue;
                };
            };
            if (e.repr == .extent) {
                if (self.table.escalate(e) == null) {
                    try self.parkAtPoolCap(io, g, &held, req, pc.block, &gate);
                    continue;
                }
                extent.stats.extent_escalations.inc();
            }
            // I. The staged sibling supersede is the writeback module's
            //    ring key; the RAM entry outranks a spilled copy.
            // J. The merge — THE one userspace copy.
            // A merge that needs a fresh cell (a shared one copied away, the
            // seed's complement) meets the same cap: the gate, then the ladder.
            const verdict = self.recordPiece(e, pc, data, req) catch |err| switch (err) {
                error.PoolExhausted => {
                    try self.parkAtPoolCap(io, g, &held, req, pc.block, &gate);
                    continue;
                },
                error.InputOutput, error.OutOfMemory => |x| return x,
            };
            e = self.table.get(req.ino, pc.block).?;
            stats.admit_write_accumulate_bytes.add(pc.len);
            // K/L. Write-through or park.
            if (verdict.completed) {
                hint = .upload;
                break :blk .write_through;
            }
            break :blk .accumulate_park;
        };
        // The guard is dropped: act on the hint.
        switch (hint) {
            .none => {},
            .overlay_drain => self.overlay.spawnDrain(io, &self.locks, req.ino, pc.block, self.closeSeams()),
            .upload => {
                // The governor permit AFTER the guard (a parked writer holds no block lock).
                upload_permit = switch (self.cfg.depth_override) {
                    .sync_inline => null,
                    else => self.governor.admitSegment(io, bs),
                };
                prof.mark(.admit_wait);
                self.spawnUpload(io, req.ino, pc.block, upload_permit, prof);
            },
            .fold => {
                stats.parked_gate_self_flushes.inc();
                _ = accumulate.flushParkedBlock(&self.table, self.uploadDeps(), io, req.ino, pc.block);
            },
        }
        return arm;
    }

    /// Arm J with its seed (D's base): the deferred complement comes from
    /// the write-side authority's key, a fresh buffer zero-completes.
    fn recordPiece(self: *WritePath, e: *accumulate.Entry, pc: Piece, data: []const u8, req: Request) error{ PoolExhausted, InputOutput, OutOfMemory }!accumulate.RecordVerdict {
        return self.table.recordWrite(e, pc.rel, data) catch |err| switch (err) {
            error.NeedsSeed => {
                const old = self.epochs.baseBinding(self.map, req.ino, pc.block) orelse {
                    e.seed = .fresh;
                    self.table.zeroComplete(e);
                    return self.table.recordWrite(e, pc.rel, data) catch |e2| switch (e2) {
                        error.PoolExhausted => error.PoolExhausted,
                        error.NeedsSeed => unreachable, // zero-completed above
                    };
                };
                const img = self.gpa.alignedAlloc(u8, .fromByteUnits(PAGE), @intCast(self.cfg.block_size)) catch return error.OutOfMemory;
                defer self.gpa.free(img);
                self.device.read(old.vol_tag, old.offset, img) catch return error.InputOutput;
                self.table.fillComplementFrom(e, img) catch |e2| switch (e2) {
                    error.PoolExhausted => return error.PoolExhausted,
                    error.NeedsSeed => unreachable, // the fill IS the seed
                };
                return self.table.recordWrite(e, pc.rel, data) catch |e2| switch (e2) {
                    error.PoolExhausted => error.PoolExhausted,
                    error.NeedsSeed => unreachable, // seeded above
                };
            },
            error.PoolExhausted => error.PoolExhausted,
        };
    }

    /// One write's standing at the gate across its ladder rounds: when its
    /// first FAILED cold flush happened. The wall runs from there.
    const GateWall = struct {
        failed_since_ns: u64 = 0,
    };

    /// An accumulation arm met the parked-buffer cap under (3): drop the
    /// guard, run the gate, and let the caller restart the ladder — or
    /// answer ENOSPC once the plane has refused the gate's flushes for a
    /// whole wall.
    fn parkAtPoolCap(self: *WritePath, io: Io, g: seams.BlockLocks.Guard, held: *bool, req: Request, block: u32, gate: *GateWall) error{ NoSpaceLeft, WriterGuardFenced }!void {
        held.* = false;
        g.release(io);
        return self.awaitParkedCapacity(io, req, block, gate);
    }

    /// THE PARKED-BUFFER GATE (the charter's backpressure law: park, never
    /// refuse, never a lost byte), holding no lock. Register on the table's
    /// capacity notify (an entry retired), re-check for room (the notify
    /// law: a retire landing between the check and the wait is never
    /// lost), then:
    ///   - room: go;
    ///   - the coldest parked block (not this one, not mid-upload) has sat
    ///     untouched ≥ `PARKED_COLD_NS`, or NO upload is in flight to return
    ///     a cell: flush it — the writer pays for one bounded upload instead
    ///     of waiting on pieces that are not coming
    ///     (`parked_gate_cold_flushes`); a flush that leaves it parked (a
    ///     failed DMA, a full set) falls through to the wait so a dead
    ///     victim is never spun on;
    ///   - else wait for the next retire, bounded by the moment the coldest
    ///     block turns cold (so a wait never outlives the flush it defers
    ///     to) or one tick when nothing is parked at all.
    /// Each visit counts `parked_gate_waits`; the op watchdog sees the
    /// station through `park_hook`. A cold flush the PLANE refuses (no
    /// fresh block to mint, a failed DMA) retires nothing and wakes nobody:
    /// one try per tick, and once the refusals have spanned
    /// `PARKED_GATE_WALL_NS` with no capacity returned the write answers
    /// ENOSPC — the plane is broken, not slow (the allocation layer's own
    /// wall, `parked_gate_wall_refusals`, one loud line). The venue's
    /// aging store reached this with every mintable block held by open
    /// rewrite-shadow epochs and reclaim debt while fill ratios read 50 %
    /// and 17 % (rung 9, the bracket harness's seq-write-1m-buffered leg).
    fn awaitParkedCapacity(self: *WritePath, io: Io, req: Request, block: u32, gate: *GateWall) error{ NoSpaceLeft, WriterGuardFenced }!void {
        stats.parked_gate_waits.inc();
        if (req.park_hook) |h| h.parked(h.ctx);
        var room = self.table.capacity.notified();
        _ = room.enable(io);
        if (self.table.hasRoom()) {
            room.cancel(io);
            gate.failed_since_ns = 0;
            return;
        }
        var until = PARKED_COLD_NS;
        if (self.table.coldestParked(req.ino, block, clock.monotonicNs())) |v| {
            if (v.age_ns >= PARKED_COLD_NS or !self.table.anyUploadInflight()) {
                stats.parked_gate_cold_flushes.inc();
                switch (accumulate.flushParkedBlock(&self.table, self.uploadDeps(), io, v.ino, v.block)) {
                    .published, .discarded, .joined => {
                        room.cancel(io);
                        gate.failed_since_ns = 0;
                        return;
                    },
                    // The writer is fenced: nothing of this mount lands again.
                    .fence_drop => {
                        room.cancel(io);
                        return error.WriterGuardFenced;
                    },
                    .stay_parked, .superseded => {
                        const now = clock.monotonicNs();
                        if (gate.failed_since_ns == 0) gate.failed_since_ns = now;
                        if (now - gate.failed_since_ns >= PARKED_GATE_WALL_NS) {
                            room.cancel(io);
                            stats.parked_gate_wall_refusals.inc();
                            log.err("write ino {d} block {d}: the parked-buffer gate's cold flushes have been refused by the plane for {d} ms (wall {d} ms; {d} parked, {d} B) — answering ENOSPC; the plane is broken, not slow", .{ req.ino, block, (now - gate.failed_since_ns) / std.time.ns_per_ms, PARKED_GATE_WALL_NS / std.time.ns_per_ms, self.table.parkedCount(), accumulate.stats.parked_full_buffer_bytes.load() });
                            return error.NoSpaceLeft;
                        }
                    },
                }
            } else until = PARKED_COLD_NS - v.age_ns;
        }
        _ = room.waitUntilUncancelable(io, clock.monotonicNs() + until);
    }

    fn spawnUpload(self: *WritePath, io: Io, ino: u64, block: u32, permit: ?pipeline.Permit, prof: pipeline.Prof) void {
        if (self.cfg.depth_override == .sync_inline) {
            var p = prof;
            self.runUpload(io, ino, block, null, &p);
            return;
        }
        self.tasks.spawn(io, uploadTask, .{ self, io, ino, block, permit, prof }) catch {
            var p = prof;
            self.runUpload(io, ino, block, permit, &p);
        };
    }

    fn uploadTask(self: *WritePath, io: Io, ino: u64, block: u32, permit: ?pipeline.Permit, prof: pipeline.Prof) Io.Cancelable!void {
        var p = prof;
        self.runUpload(io, ino, block, permit, &p);
    }

    fn runUpload(self: *WritePath, io: Io, ino: u64, block: u32, permit: ?pipeline.Permit, prof: *pipeline.Prof) void {
        prof.mark(.detach_lag);
        const d = accumulate.uploadParkedBlock(&self.table, self.uploadDeps(), io, ino, block, prof);
        if (permit) |p| {
            const ord: u32 = if (self.map.lookup(ino, block)) |b| (if (self.vols.byTag(b.vol_tag)) |v| v.ordinal else 0) else 0;
            self.governor.complete(io, p, ord, clock.monotonicNs() -| p.admitted_ns);
        }
        prof.finish();
        pipeline.countDisposition(d, self.cfg.block_size);
    }

    /// Wait for every detached upload/drain (tests, teardown).
    pub fn quiesce(self: *WritePath, io: Io) void {
        self.tasks.wait(io) catch {};
        self.overlay.tasks.wait(io) catch {};
    }

    /// RELEASE / unmount: every parked block of `ino` flushes durably, the
    /// overlay drains (write-back class), the epoch closes.
    pub fn flushIno(self: *WritePath, io: Io, ino: u64) void {
        _ = self.overlay.drainForIno(io, &self.locks, ino, std.math.maxInt(u64), false, self.closeSeams());
        var blocks: [64]u32 = undefined;
        for (self.table.blocksOf(ino, &blocks)) |b| _ = accumulate.flushParkedBlock(&self.table, self.uploadDeps(), io, ino, b);
        _ = self.epochs.close(io, ino, self.closeSeams());
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const wt = @import("write_testing.zig");

test "the splitter: pieces are single-block, page offsets preserved; a straddling write of a skewed grid yields one piece per block touched" {
    var buf: [8]Piece = undefined;
    const bs: u64 = 16 * PAGE;
    const p1 = splitPieces(0, bs, bs, &buf);
    try testing.expectEqual(@as(usize, 1), p1.len);
    const p2 = splitPieces(bs - 3 * PAGE, 6 * PAGE, bs, &buf);
    try testing.expectEqual(@as(usize, 2), p2.len);
    try testing.expectEqual(Piece{ .block = 0, .rel = @intCast(bs - 3 * PAGE), .off = 0, .len = 3 * PAGE }, p2[0]);
    try testing.expectEqual(Piece{ .block = 1, .rel = 0, .off = 3 * PAGE, .len = 3 * PAGE }, p2[1]);
    const p3 = splitPieces(100, 3 * bs, bs, &buf); // skewed: 4 blocks touched
    try testing.expectEqual(@as(usize, 4), p3.len);
    try testing.expectEqual(@as(u32, 100), p3[0].rel);
    try testing.expectEqual(@as(u32, 100), p3[3].len);
    try testing.expectEqual(@as(usize, 5), maxPieces(3 * bs, bs));
    var total: u64 = 0;
    for (p3) |p| total += p.len;
    try testing.expectEqual(3 * bs, total);
}

test "last_write_end is a bounded striped word swapped unconditionally; adjacency = prev end == offset" {
    var t = try LastWriteEnd.init(testing.allocator, 16);
    defer t.deinit(testing.allocator);
    try testing.expectEqual(@as(u64, 0), t.note(7, 4096, 4096));
    try testing.expectEqual(@as(u64, 8192), t.note(7, 8192, 4096)); // adjacent
    try testing.expectEqual(@as(u64, 12288), t.note(7, 0, 4096)); // not adjacent
    try testing.expectEqual(@as(usize, 16), t.words.len); // bounded, never a per-ino map
}

test "Config.fromEnv reads every row once: defaults derive; levers log when non-default; an override wins verbatim" {
    var env: std.process.Environ.Map = .init(testing.allocator);
    defer env.deinit();
    const c = Config.fromEnv(&env, 4 << 20, 64 * derive.GiB);
    try testing.expectEqual(@as(u64, 512 * 1024), c.patch_max);
    try testing.expect(c.overlay.enabled and c.overlay.overwrite and !c.overlay.close_barrier and c.overlay.depth_governor);
    try testing.expect(c.rewrite_shadow);
    try testing.expectEqual(@as(u64, 64), c.fold_max_extents);
    try testing.expectEqual(@as(u64, 1 << 20), c.fold_max_bytes);
    try testing.expectEqual(@as(u64, 1024), c.parked_cap);
    try testing.expectEqual(pipeline.DepthOverride.adaptive, c.depth_override);
    try testing.expect(c.fsync.touched_namespaces and c.fsync.parallel_legs and !c.fsync.promote_staged.on);
    try env.put("SQUEEZEFS_PATCH_MAX_BYTES", "0");
    try env.put("SQUEEZEFS_DEVICE_OVERLAY", "0");
    try env.put("SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS", "0");
    const c2 = Config.fromEnv(&env, 4 << 20, 64 * derive.GiB);
    try testing.expectEqual(@as(u64, 0), c2.patch_max);
    try testing.expect(!c2.overlay.enabled);
    try testing.expectEqual(pipeline.DepthOverride.sync_inline, c2.depth_override);
}

const World = struct {
    f: *wt.Fixture,
    stripes: layout.MetaStripes = .{},
    wp: *WritePath,

    fn init(gpa: Allocator, depth: pipeline.DepthOverride, overlay_on: bool) !*World {
        const w = try gpa.create(World);
        errdefer gpa.destroy(w);
        w.f = try wt.Fixture.init(gpa, 32);
        w.stripes = .{};
        const cfg: Config = .{
            .block_size = wt.BS,
            .patch_max = wt.BS / 8,
            .overlay = .{ .block_size = wt.BS, .patch_max = wt.BS / 8, .enabled = overlay_on },
            .rewrite_shadow = true,
            // At the 64 KiB test block the derivations fold at 1 extent; the shipped shape is pinned here.
            .fold_max_extents = 64,
            .fold_max_bytes = wt.BS / 4,
            .parked_cap = 8,
            .depth_override = depth,
            .fsync = .{},
        };
        w.wp = try WritePath.init(gpa, cfg, .{ .io = testing.io, .vols = &w.f.vols, .device = w.f.dev.seam(), .map = w.f.map.seam(), .free = w.f.frees.seam(), .stripes = &w.stripes, .budget_bytes = 1 << 30 });
        return w;
    }

    fn deinit(self: *World, gpa: Allocator) void {
        self.wp.deinit(testing.io);
        self.f.deinit();
        gpa.destroy(self);
    }

    fn striped(size: u64) seams.FileView {
        return .{ .kind = .striped, .size = size, .map_resident = true };
    }
};

fn fill(buf: []u8, seed: u8) void {
    for (buf, 0..) |*b, i| b.* = seed +% @as(u8, @truncate(i * 13));
}

test "the admission ledger identity: Σ admit_write_* ≡ user bytes at ACK across every arm (overlay, in-place patch, accumulate, inline)" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, true);
    defer w.deinit(testing.allocator);
    const sum0 = stats.ledgerSum();
    const viol0 = counters.tripwireCount("write_shared_class_violation");
    var user: u64 = 0;
    // Overlay: a whole-block aligned fresh write (ino 1).
    try w.f.map.create(1, .striped, 0);
    var whole: [wt.BS]u8 align(4096) = undefined;
    fill(&whole, 1);
    const o1 = try w.wp.write(io, .{ .ino = 1, .offset = 0, .payload = .{ .bytes = &whole }, .class = .entire_op, .file = World.striped(0) });
    user += o1.written;
    try testing.expectEqual(@as(u32, 1), o1.count(.overlay));
    w.wp.quiesce(io);
    // In-place patch: 4 KiB aligned into a published block (ino 2).
    _ = try w.f.publishBlock(2, 0, 0xAA);
    var page: [PAGE]u8 align(4096) = @splat(0x22);
    const o2 = try w.wp.write(io, .{ .ino = 2, .offset = 3 * PAGE, .payload = .{ .bytes = &page }, .class = .shared, .file = World.striped(wt.BS) });
    user += o2.written;
    try testing.expectEqual(@as(u32, 1), o2.count(.patch));
    // Accumulate: a half-block unaligned-length piece parks; its sibling completes and writes through (ino 3).
    try w.f.map.create(3, .striped, 0);
    var half: [wt.BS / 2 + 100]u8 = undefined;
    fill(&half, 3);
    const o3 = try w.wp.write(io, .{ .ino = 3, .offset = 0, .payload = .{ .bytes = &half }, .class = .entire_op, .file = World.striped(0) });
    user += o3.written;
    try testing.expectEqual(@as(u32, 1), o3.count(.accumulate_park));
    var rest: [wt.BS / 2 - 100]u8 = undefined;
    fill(&rest, 4);
    const o4 = try w.wp.write(io, .{ .ino = 3, .offset = wt.BS / 2 + 100, .payload = .{ .bytes = &rest }, .class = .entire_op, .file = World.striped(wt.BS / 2 + 100) });
    user += o4.written;
    try testing.expectEqual(@as(u32, 1), o4.count(.write_through));
    // Inline: the router's arm.
    noteInlineWrite(777);
    user += 777;
    try testing.expectEqual(user, stats.ledgerSum() - sum0);
    // Byte exactness of every arm's bytes on the device.
    var got: [wt.BS]u8 = undefined;
    const b1 = w.f.map.seam().lookup(1, 0).?;
    try testing.expectEqual(wt.VOL_A, b1.vol_tag); // the overlay lands on a passthrough volume only
    w.f.dev.peek(b1.vol_tag, b1.offset, &got);
    try testing.expectEqualSlices(u8, &whole, &got);
    w.f.dev.peek(wt.VOL_A, w.f.map.seam().lookup(2, 0).?.offset, &got);
    try testing.expect(std.mem.allEqual(u8, got[3 * PAGE .. 4 * PAGE], 0x22));
    try testing.expect(std.mem.allEqual(u8, got[0 .. 3 * PAGE], 0xAA));
    const b3 = w.f.map.seam().lookup(3, 0).?;
    w.f.dev.peek(b3.vol_tag, b3.offset, &got);
    try testing.expectEqualSlices(u8, &half, got[0..half.len]);
    try testing.expectEqualSlices(u8, &rest, got[half.len..]);
    try testing.expectEqual(viol0, counters.tripwireCount("write_shared_class_violation"));
}

test "the ENOSPC terminal law: a write that would CREATE custody against an exhausted set is refused BEFORE any byte is recorded; custody already held (a parked block's sibling, a brim rewrite of a sole-owned mapped block) keeps the ladder; the latch clears when supply returns" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, true);
    defer w.deinit(testing.allocator);
    // Park a partial block of ino 10 (custody held), publish a block of ino 11.
    try w.f.map.create(10, .striped, 0);
    var half: [wt.BS / 2]u8 = @splat(0x10);
    _ = try w.wp.write(io, .{ .ino = 10, .offset = 0, .payload = .{ .bytes = &half }, .class = .entire_op, .file = World.striped(0) });
    _ = try w.f.publishBlock(11, 0, 0xBB);
    // Exhaust both volumes.
    while (w.f.alloc_a.allocateBlock()) |_| {} else |_| {}
    while (w.f.alloc_b.allocateBlock()) |_| {} else |_| {}
    var cands: [router.MAX_VOLUMES]*block_alloc.BlockAllocator = undefined;
    try testing.expect(enospc.freshSupplyExhausted(w.f.vols.candidates(&cands)));
    // Fresh custody: refused before any byte, nothing parked.
    try w.f.map.create(12, .striped, 0);
    const parked0 = w.wp.table.parkedCount();
    const ref0 = stats.write_enospc_refusals.load();
    try testing.expectError(error.NoSpaceLeft, w.wp.write(io, .{ .ino = 12, .offset = 0, .payload = .{ .bytes = &half }, .class = .entire_op, .file = World.striped(0) }));
    try testing.expectEqual(ref0 + 1, stats.write_enospc_refusals.load());
    try testing.expectEqual(parked0, w.wp.table.parkedCount());
    try testing.expect(!w.wp.table.has(12, 0));
    // A hole below EOF of a pre-sized file holds nothing: refused.
    try w.f.map.create(13, .striped, 4 * wt.BS);
    try testing.expectError(error.NoSpaceLeft, w.wp.write(io, .{ .ino = 13, .offset = wt.BS, .payload = .{ .bytes = &half }, .class = .entire_op, .file = World.striped(4 * wt.BS) }));
    // Custody held: the parked block's sibling keeps the ladder (acked).
    const o = try w.wp.write(io, .{ .ino = 10, .offset = wt.BS / 2, .payload = .{ .bytes = half[0 .. wt.BS / 4] }, .class = .entire_op, .file = World.striped(wt.BS / 2) });
    try testing.expectEqual(@as(u64, wt.BS / 4), o.written);
    // A brim rewrite of a sole-owned passthrough mapped block: admitted (lands in place).
    var page: [PAGE]u8 align(4096) = @splat(0x44);
    const o2 = try w.wp.write(io, .{ .ino = 11, .offset = PAGE, .payload = .{ .bytes = &page }, .class = .shared, .file = World.striped(wt.BS) });
    try testing.expectEqual(@as(u32, 1), o2.count(.patch));
    // Supply returns: the refusal lifts.
    _ = try w.f.alloc_a.freeBlock(5);
    try testing.expect(!enospc.freshSupplyExhausted(w.f.vols.candidates(&cands)));
    const o3 = try w.wp.write(io, .{ .ino = 12, .offset = 0, .payload = .{ .bytes = &half }, .class = .entire_op, .file = World.striped(0) });
    try testing.expectEqual(@as(u64, wt.BS / 2), o3.written);
}

test "the per-piece ladder (the accumulation control arm, overlay off): out-of-order O_DIRECT-style pieces of one block complete the coverage union and write through once; a partial tail parks; W2 parks a small non-adjacent write compactly and escalates at 25 %; the shared-mode handoff trips on an extending Shared write" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, false);
    defer w.deinit(testing.allocator);
    try w.f.map.create(20, .striped, 0);
    // Three thirds, end first, middle last — one write-through, zero seed reads.
    var third: [wt.BS / 4]u8 = undefined;
    fill(&third, 7);
    _ = try w.wp.write(io, .{ .ino = 20, .offset = 3 * wt.BS / 4, .payload = .{ .bytes = &third }, .class = .entire_op, .file = World.striped(0) });
    _ = try w.wp.write(io, .{ .ino = 20, .offset = 0, .payload = .{ .bytes = &third }, .class = .entire_op, .file = World.striped(wt.BS) });
    var mid: [wt.BS / 2]u8 = undefined;
    fill(&mid, 9);
    const wt0 = accumulate.stats.write_through_blocks.load();
    const o = try w.wp.write(io, .{ .ino = 20, .offset = wt.BS / 4, .payload = .{ .bytes = &mid }, .class = .entire_op, .file = World.striped(wt.BS) });
    try testing.expectEqual(@as(u32, 1), o.count(.write_through));
    try testing.expectEqual(wt0 + 1, accumulate.stats.write_through_blocks.load());
    try testing.expectEqual(@as(u64, 0), w.f.dev.reads);
    try testing.expectEqual(@as(usize, 1), w.f.dev.writes.items.len);
    var got: [wt.BS]u8 = undefined;
    const b20 = w.f.map.seam().lookup(20, 0).?;
    w.f.dev.peek(b20.vol_tag, b20.offset, &got);
    try testing.expectEqualSlices(u8, &third, got[0 .. wt.BS / 4]);
    try testing.expectEqualSlices(u8, &mid, got[wt.BS / 4 .. 3 * wt.BS / 4]);
    try testing.expectEqualSlices(u8, &third, got[3 * wt.BS / 4 ..]);
    // A partial tail parks (the last 100 bytes of block 1).
    var tail: [100]u8 = @splat(0x55);
    const o2 = try w.wp.write(io, .{ .ino = 20, .offset = wt.BS + 1000, .payload = .{ .bytes = &tail }, .class = .entire_op, .file = World.striped(wt.BS) });
    try testing.expect(o2.count(.extent) == 1 or o2.count(.accumulate_park) == 1);
    try testing.expect(w.wp.table.has(20, 1));
    // W2: 4 KiB non-adjacent small writes into a hole block park as extents; the 25 % edge escalates.
    try w.f.map.create(21, .striped, 8 * wt.BS);
    var page: [PAGE]u8 = @splat(0x66);
    const parks0 = extent.stats.extent_parks.load();
    _ = try w.wp.write(io, .{ .ino = 21, .offset = 2 * wt.BS + 8 * PAGE, .payload = .{ .bytes = &page }, .class = .shared, .file = World.striped(8 * wt.BS) });
    _ = try w.wp.write(io, .{ .ino = 21, .offset = 2 * wt.BS + 2 * PAGE, .payload = .{ .bytes = &page }, .class = .shared, .file = World.striped(8 * wt.BS) });
    try testing.expectEqual(parks0 + 2, extent.stats.extent_parks.load());
    const e = w.wp.table.get(21, 2).?;
    try testing.expect(e.repr == .extent);
    try testing.expectEqual(@as(u64, 2 * PAGE), e.repr.extent.bytes); // a 4 KiB write parks ~4 KiB, not a block
    const gauge_before = extent.stats.parked_extent_bytes.load();
    const esc0 = extent.stats.extent_escalations.load();
    _ = try w.wp.write(io, .{ .ino = 21, .offset = 2 * wt.BS + 12 * PAGE, .payload = .{ .bytes = &page }, .class = .shared, .file = World.striped(8 * wt.BS) });
    _ = try w.wp.write(io, .{ .ino = 21, .offset = 2 * wt.BS + 14 * PAGE, .payload = .{ .bytes = &page }, .class = .shared, .file = World.striped(8 * wt.BS) }); // 4 pages = 25 % of 16
    try testing.expectEqual(esc0 + 1, extent.stats.extent_escalations.load());
    try testing.expect(w.wp.table.get(21, 2).?.repr == .full);
    try testing.expectEqual(gauge_before - 2 * PAGE, extent.stats.parked_extent_bytes.load()); // the slabs left the extent budget for the buffer's
    // The shared-mode handoff: a Shared write that extends trips the wire (the data plane still serves it).
    const viol0 = counters.tripwireCount("write_shared_class_violation");
    stats.write_shared_class_violation.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    _ = try w.wp.write(io, .{ .ino = 21, .offset = 9 * wt.BS, .payload = .{ .bytes = &page }, .class = .shared, .file = World.striped(8 * wt.BS) });
    try log.testing.expectAllFired();
    try testing.expectEqual(viol0 + 1, counters.tripwireCount("write_shared_class_violation"));
    // The flush on release lands everything.
    w.wp.flushIno(io, 20);
    w.wp.flushIno(io, 21);
    try testing.expect(!w.wp.table.has(20, 1));
    try testing.expect(!w.wp.table.has(21, 2));
    try testing.expect(w.f.map.seam().lookup(21, 2) != null);
}

test "every acked byte reads back across the W2 extent cap (repro-port: LTP fs gf27 — one-byte grow-by-lseek writes 4 KiB apart into block 1 read back 0 from the 65th on): 1-byte writes 512 B apart into one block, each served back by the write side before and after FOLD_MAX_EXTENTS; the flush lands all of them" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, false);
    defer w.deinit(testing.allocator);
    // Block 0 is mapped (the file's first block landed); block 1 is the hole
    // the grows extend into — the LTP shape past the first boundary.
    try w.f.map.create(30, .striped, 0);
    _ = try w.f.publishBlock(30, 0, 0xAA);
    const view = w.wp.writeSideView();
    const step: u64 = 512;
    const n_writes: u64 = wt.BS / step; // 128 > fold_max_extents (64)
    var size: u64 = wt.BS;
    var i: u64 = 0;
    var win: [wt.BS]u8 = undefined;
    while (i < n_writes) : (i += 1) {
        const off = wt.BS + (i + 1) * step - 1;
        const wrote = try w.wp.write(io, .{ .ino = 30, .offset = off, .payload = .{ .bytes = "w" }, .class = .entire_op, .file = World.striped(size) });
        try testing.expectEqual(@as(u64, 1), wrote.written);
        size = off + 1;
        // The read the suite does after every write: the write side's window
        // composed the way `plane.wsStagedBlock` composes it — covered runs
        // from the view, the gaps from the base binding it names (or the
        // map's block once the write side let go).
        @memset(&win, 0);
        const o = view.serve(io, .{ .ino = 30, .block = 1, .rel = 0, .out = &win });
        switch (o) {
            .none => {
                const b = w.f.map.seam().lookup(30, 1) orelse return error.TestUnexpectedResult;
                w.f.dev.peek(b.vol_tag, b.offset, &win);
            },
            .partial => |p| if (p.base) |base| {
                var img: [wt.BS]u8 = undefined;
                w.f.dev.peek(base.vol_tag, base.offset, &img);
                var runs: [cov.CoverageUnion.max_runs]cov.Run = undefined;
                const covered = p.covered.coveredRunsIn(0, wt.BS, &runs);
                var cursor: u32 = 0;
                for (covered) |r| {
                    if (r.start > cursor) @memcpy(win[cursor..r.start], img[cursor..r.start]);
                    cursor = r.end;
                }
                if (cursor < wt.BS) @memcpy(win[cursor..], img[cursor..]);
            },
            .redirect => |shadow| w.f.dev.peek(shadow.vol_tag, shadow.offset, &win),
            .served, .settle, .busy => {},
        }
        var k: u64 = 0;
        while (k <= i) : (k += 1) {
            const rel: usize = @intCast((k + 1) * step - 1);
            if (win[rel] != 'w') {
                std.debug.print("write {d}: byte at rel {d} reads {d}, not 'w' (outcome {t})\n", .{ i, rel, win[rel], o });
                return error.TestUnexpectedResult;
            }
            if (rel > 0 and win[rel - 1] != 0) return error.TestUnexpectedResult;
        }
    }
    w.wp.flushIno(io, 30);
    try testing.expect(!w.wp.table.has(30, 1));
    const b = w.f.map.seam().lookup(30, 1) orelse return error.TestUnexpectedResult;
    w.f.dev.peek(b.vol_tag, b.offset, &win);
    i = 0;
    while (i < n_writes) : (i += 1) try testing.expectEqual(@as(u8, 'w'), win[@intCast((i + 1) * step - 1)]);
}

test "fsync through the write path: a parked block, an open overlay and a shadowed epoch all land; the phases sum exactly; nothing stays open" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, true);
    defer w.deinit(testing.allocator);
    _ = try w.f.publishBlock(30, 0, 0xAA);
    var half: [wt.BS / 2]u8 align(4096) = @splat(0x30);
    // An overwrite half-block: overlay (overwrite shape) → fed on drain.
    _ = try w.wp.write(io, .{ .ino = 30, .offset = 0, .payload = .{ .bytes = &half }, .class = .shared, .file = World.striped(wt.BS) });
    // A fresh partial block parks.
    var tail: [1000]u8 = @splat(0x31);
    _ = try w.wp.write(io, .{ .ino = 30, .offset = 2 * wt.BS, .payload = .{ .bytes = &tail }, .class = .entire_op, .file = World.striped(wt.BS) });
    const unpub0 = counters.tripwireCount("overlay_unpublished_at_fsync");
    var prof = fsync_mod.Prof.start();
    try fsync_mod.fsync(w.wp.fsyncCtx(), io, 30, &prof);
    prof.finish();
    var sum: u64 = 0;
    for (prof.legs) |l| sum += l;
    try testing.expectEqual(prof.total(), sum);
    try testing.expect(!w.wp.overlay.anyOpenFast());
    try testing.expectEqual(@as(usize, 0), w.wp.epochs.openCount());
    try testing.expect(!w.wp.table.has(30, 2));
    try testing.expect(w.f.map.seam().lookup(30, 2) != null);
    var got: [wt.BS]u8 = undefined;
    const b30 = w.f.map.seam().lookup(30, 0).?;
    w.f.dev.peek(b30.vol_tag, b30.offset, &got);
    try testing.expect(std.mem.allEqual(u8, got[0 .. wt.BS / 2], 0x30));
    try testing.expect(std.mem.allEqual(u8, got[wt.BS / 2 ..], 0xAA));
    try testing.expect(w.f.dev.flushes.items.len >= 1);
    try testing.expectEqual(unpub0, counters.tripwireCount("overlay_unpublished_at_fsync"));
}

test "the lost overwrite (2026-10-07, the smoke's dd-per-overwrite shape): a write landing on a block the OPEN epoch shadows takes the accumulation arm with the epoch's B as its base, never the map's stale A; after the save, the next flush seeds from B and its save displaces exactly what it parked — every acked byte readable, drift 0, nothing freed twice" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, true);
    defer w.deinit(testing.allocator);
    // A two-block striped file: block 1's size floor exceeds one block, so
    // a feed over it never auto-closes the epoch (the smoke's `big`).
    _ = try w.f.publishBlock(50, 0, 0xAA);
    const a1 = try w.f.publishBlock(50, 1, 0xBB);
    const file = World.striped(2 * wt.BS);
    const drift0 = counters.tripwireCount("rewrite_shadow_displaced_drift");
    const untracked0 = counters.tripwireCount("block_untracked_free_refusals");
    // dd #N: an unaligned 4 KiB overwrite of block 1 (W1-ineligible ⇒ W2 park).
    var p: [PAGE]u8 = @splat(0x11);
    const o1 = try w.wp.write(io, .{ .ino = 50, .offset = wt.BS + 3 * PAGE + 100, .payload = .{ .bytes = &p }, .class = .shared, .file = file });
    try testing.expectEqual(@as(u32, 1), o1.count(.extent));
    // RELEASE #N, first half: the parked block flushes and FEEDS the epoch
    // (shadow B over block 1, A1 parked); the ONE save has not run yet.
    try testing.expectEqual(accumulate.Disposition.published, accumulate.flushParkedBlock(&w.wp.table, w.wp.uploadDeps(), io, 50, 1));
    try testing.expect(w.wp.epochs.bindsBlock(50, 1));
    const b = w.wp.epochs.shadowOf(50, 1).?;
    try testing.expect(w.f.map.seam().lookup(50, 1).?.eql(a1));
    // dd #N+1's WRITE lands meanwhile: an aligned page of block 1. W1 and
    // the overlay decline (the epoch binds the block); the park's base is B.
    var q: [PAGE]u8 align(4096) = @splat(0x22);
    const o2 = try w.wp.write(io, .{ .ino = 50, .offset = wt.BS + 8 * PAGE, .payload = .{ .bytes = &q }, .class = .shared, .file = file });
    try testing.expectEqual(@as(u32, 0), o2.count(.patch));
    try testing.expectEqual(@as(u32, 0), o2.count(.overlay));
    try testing.expect(w.wp.table.get(50, 1).?.old_binding.?.eql(b));
    // A read of the parked block's gaps composes over B, not A1.
    var gap: [PAGE]u8 = undefined;
    const served = w.wp.writeSideView().serve(io, .{ .ino = 50, .block = 1, .rel = 3 * PAGE, .out = &gap });
    try testing.expect(served == .partial and served.partial.base.?.eql(b));
    // RELEASE #N, second half: the save lands (map → B, A1 freed once).
    try testing.expect(w.wp.epochs.close(io, 50, w.wp.closeSeams()) == .swapped);
    try testing.expect(w.f.map.seam().lookup(50, 1).?.eql(b));
    // RELEASE #N+1: the flush seeds from B, feeds, and the save displaces B.
    w.wp.flushIno(io, 50);
    const final = w.f.map.seam().lookup(50, 1).?;
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(final.vol_tag, final.offset, &got);
    try testing.expect(std.mem.allEqual(u8, got[3 * PAGE + 100 .. 4 * PAGE + 100], 0x11)); // dd #N's bytes
    try testing.expect(std.mem.allEqual(u8, got[8 * PAGE .. 9 * PAGE], 0x22)); // dd #N+1's bytes
    try testing.expect(std.mem.allEqual(u8, got[0 .. 3 * PAGE], 0xBB));
    try testing.expect(std.mem.allEqual(u8, got[9 * PAGE ..], 0xBB));
    try testing.expectEqual(@as(usize, 2), w.f.frees.freed.items.len);
    try testing.expect(w.f.frees.freed.items[0].eql(a1));
    try testing.expect(w.f.frees.freed.items[1].eql(b));
    try testing.expectEqual(drift0, counters.tripwireCount("rewrite_shadow_displaced_drift"));
    try testing.expectEqual(untracked0, counters.tripwireCount("block_untracked_free_refusals"));
    try testing.expectEqual(@as(usize, 0), w.wp.epochs.openCount());
    try testing.expectEqual(@as(u64, 2), w.f.alloc_a.usedBlocks() + w.f.alloc_b.usedBlocks()); // the file's two blocks, nothing leaked
}

test "the save window: while an epoch's ONE save is in flight the block stays bound — a racing W1 patch declines (it would land in the key the save displaces), the overlay declines, the park bases on B; the save displaces only what it parked" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, true);
    defer w.deinit(testing.allocator);
    _ = try w.f.publishBlock(51, 0, 0xAA);
    const a1 = try w.f.publishBlock(51, 1, 0xBB);
    const drift0 = counters.tripwireCount("rewrite_shadow_displaced_drift");
    const untracked0 = counters.tripwireCount("block_untracked_free_refusals");
    // Shadow block 1 with a minted B: A1's image with page 3 rewritten.
    var img: [wt.BS]u8 = @splat(0xBB);
    @memset(img[3 * PAGE .. 4 * PAGE], 0x11);
    const m = try w.f.alloc_a.allocateBlock();
    const proof = try w.f.dev.seam().write(wt.VOL_A, m.offset, &img);
    w.f.alloc_a.publishBlock(m.idx);
    const b = Binding.wholeBlock(wt.VOL_A, m.offset, m.stamp);
    _ = try w.wp.epochs.record(51, 1, a1, b, proof, 2 * wt.BS, block_alloc.InflightAllocGuard.register(w.f.alloc_a, m.idx), true);
    // The racing WRITE runs inside the save's window: an aligned page (the
    // W1 shape) and a half-block aligned piece (the overlay shape).
    const Racer = struct {
        var wp: *WritePath = undefined;
        var page_arms: Outcome = undefined;
        var half_arms: Outcome = undefined;
        var page: [PAGE]u8 align(4096) = @splat(0x22);
        var half: [wt.BS / 2]u8 align(4096) = @splat(0x33);
        fn hook(_: *anyopaque, _: *wt.RamMap) void {
            page_arms = wp.write(testing.io, .{ .ino = 51, .offset = wt.BS + 8 * PAGE, .payload = .{ .bytes = &page }, .class = .shared, .file = World.striped(2 * wt.BS) }) catch unreachable;
            half_arms = wp.write(testing.io, .{ .ino = 51, .offset = wt.BS, .payload = .{ .bytes = &half }, .class = .shared, .file = World.striped(2 * wt.BS) }) catch unreachable;
        }
    };
    Racer.wp = w.wp;
    var dummy: u8 = 0;
    w.f.map.before_merge = .{ .ctx = &dummy, .call = &Racer.hook };
    try testing.expect(w.wp.epochs.close(io, 51, w.wp.closeSeams()) == .swapped);
    try testing.expectEqual(@as(u32, 0), Racer.page_arms.count(.patch));
    try testing.expectEqual(@as(u32, 0), Racer.half_arms.count(.overlay));
    try testing.expect(w.f.map.seam().lookup(51, 1).?.eql(b));
    try testing.expect(w.wp.table.get(51, 1).?.old_binding.?.eql(b));
    w.wp.flushIno(io, 51);
    const final = w.f.map.seam().lookup(51, 1).?;
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(final.vol_tag, final.offset, &got);
    try testing.expect(std.mem.allEqual(u8, got[0 .. wt.BS / 2], 0x33));
    try testing.expect(std.mem.allEqual(u8, got[8 * PAGE .. 9 * PAGE], 0x22));
    try testing.expect(std.mem.allEqual(u8, got[9 * PAGE ..], 0xBB));
    try testing.expectEqual(@as(usize, 2), w.f.frees.freed.items.len);
    try testing.expect(w.f.frees.freed.items[0].eql(a1));
    try testing.expect(w.f.frees.freed.items[1].eql(b));
    try testing.expectEqual(drift0, counters.tripwireCount("rewrite_shadow_displaced_drift"));
    try testing.expectEqual(untracked0, counters.tripwireCount("block_untracked_free_refusals"));
    try testing.expectEqual(@as(u64, 2), w.f.alloc_a.usedBlocks() + w.f.alloc_b.usedBlocks());
}

test "the one-authority screen never parks under (3): a write meeting an overlay record whose settle keeps failing drops the block guard for the settle's ladder (a sibling takes the guard mid-ladder) and restarts its ladder once the record landed — the write acks, the record published" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, true);
    defer w.deinit(testing.allocator);
    try w.f.map.create(70, .striped, 0);
    // An open record with half the block covered (the gap seed is the leg
    // that will keep failing).
    var half: [wt.BS / 2]u8 align(4096) = undefined;
    fill(&half, 7);
    const o1 = try w.wp.write(io, .{ .ino = 70, .offset = 0, .payload = .{ .bytes = &half }, .class = .entire_op, .file = World.striped(0) });
    try testing.expectEqual(@as(u32, 1), o1.count(.overlay));
    w.f.dev.setFailWrites(std.math.maxInt(u32), error.InputOutput);
    const r0 = device_overlay.stats.overlay_settle_retries.load();
    const pub0 = device_overlay.stats.overlay_publishes.load();
    // A page-misaligned piece: overlay-ineligible, patch-ineligible — it
    // reaches the one-authority screen and must settle the record first.
    const Writer = struct {
        fn run(world: *World, io_: Io, out: *?Outcome) void {
            var small: [PAGE + 12]u8 = undefined;
            fill(&small, 9);
            out.* = world.wp.write(io_, .{ .ino = 70, .offset = wt.BS / 2 + 100, .payload = .{ .bytes = &small }, .class = .entire_op, .file = World.striped(wt.BS / 2) }) catch null;
        }
    };
    var outcome: ?Outcome = null;
    const t = try std.Thread.spawn(.{}, Writer.run, .{ w, io, &outcome });
    var deadline = clock.monotonicNs() + 10 * std.time.ns_per_s;
    while (device_overlay.stats.overlay_settle_retries.load() < r0 + 1) {
        if (clock.monotonicNs() > deadline) break;
        try std.Thread.yield();
    }
    var guard: ?seams.BlockLocks.Guard = null;
    deadline = clock.monotonicNs() + 3 * std.time.ns_per_s;
    while (guard == null and clock.monotonicNs() < deadline) {
        guard = w.wp.locks.tryAcquire(io, 70, 0);
        if (guard == null) try std.Thread.yield();
    }
    const taken = guard != null;
    w.f.dev.setFailWrites(0, error.InputOutput);
    if (guard) |g| g.release(io);
    t.join();
    try testing.expect(taken);
    const o2 = outcome orelse return error.TestUnexpectedResult;
    try testing.expectEqual(@as(u64, PAGE + 12), o2.written);
    try testing.expectEqual(pub0 + 1, device_overlay.stats.overlay_publishes.load());
    const b = w.f.map.seam().lookup(70, 0) orelse return error.TestUnexpectedResult;
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(b.vol_tag, b.offset, &got);
    try testing.expectEqualSlices(u8, &half, got[0 .. wt.BS / 2]);
    try testing.expectEqual(@as(u32, 0), w.wp.overlay.registry.open_count.load(.monotonic));
}

/// Half a block (a park, never a write-through) for `(ino, block)`.
fn parkHalf(w: *World, io: Io, ino: u64, block: u32, seed: u8) !void {
    var half: [wt.BS / 2]u8 align(4096) = undefined;
    fill(&half, seed);
    if (w.f.map.files.get(ino) == null) try w.f.map.create(ino, .striped, 0);
    const o = try w.wp.write(io, .{ .ino = ino, .offset = @as(u64, block) * wt.BS, .payload = .{ .bytes = &half }, .class = .entire_op, .file = World.striped(@as(u64, block) * wt.BS) });
    try testing.expectEqual(@as(u32, 1), o.count(.accumulate_park));
}

// Rung 9 F2: at the parked-buffer cap the accumulation checkout answered
// WouldBlock, which the handler sent to a BLOCKING write(2) as EAGAIN (fio
// aborted on 100–128 per 60 s leg; the venue's pool sat full of cold
// partials — 250 parked, 36 uploads). A blocking write on a regular file
// may not answer EAGAIN: the gate parks, and a set that is COLD (nothing
// in flight to return a cell) is drained by the writer itself.
test "the parked-buffer gate, cold set: the write past the cap completes at once — the coldest parked block is flushed durably (never a lost byte), the piece parks, no EAGAIN" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, false);
    defer w.deinit(testing.allocator);
    const waits0 = stats.parked_gate_waits.load();
    const cold0 = stats.parked_gate_cold_flushes.load();
    // Eight parked partials fill the cap (ino 1, blocks 0..7); block 0 is
    // the coldest.
    for (0..8) |b| try parkHalf(w, io, 1, @intCast(b), @intCast(b + 1));
    try testing.expectEqual(@as(u32, 8), w.wp.table.parkedCount());
    try testing.expect(!w.wp.table.hasRoom());
    const t0 = clock.monotonicNs();
    try parkHalf(w, io, 1, 8, 9);
    const waited = clock.monotonicNs() - t0;
    try testing.expect(stats.parked_gate_waits.load() > waits0);
    try testing.expectEqual(cold0 + 1, stats.parked_gate_cold_flushes.load());
    // No upload was in flight to return a cell: the coldest block was
    // flushed at once, never after a tick of waiting on nothing.
    try testing.expect(waited < PARKED_COLD_NS / 2);
    // Block 0 is on the device: its half, then zeros; blocks 1..8 parked.
    const b0 = w.f.map.seam().lookup(1, 0) orelse return error.VictimNotPublished;
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(b0.vol_tag, b0.offset, &got);
    var half: [wt.BS / 2]u8 = undefined;
    fill(&half, 1);
    try testing.expectEqualSlices(u8, &half, got[0 .. wt.BS / 2]);
    try testing.expect(std.mem.allEqual(u8, got[wt.BS / 2 ..], 0));
    try testing.expectEqual(@as(u32, 8), w.wp.table.parkedCount());
    try testing.expect(!w.wp.table.has(1, 0));
    try testing.expect(w.wp.table.has(1, 8));
}

// A dead victim: the coldest block's flush fails (no fresh block, a failed
// DMA) — the gate waits a whole tick before trying again, woken only by a
// RETIRE (capacity returned), never by an upload's exit: a failed upload
// settles `upload_settled` too, and three writers at the cap riding that
// notify woke one another's failed flushes forever (the bracket harness's
// seq-write-1m-buffered leg: 7.7 M, then 3.75 M gate rounds in four
// minutes on a set whose free blocks all sat in reclaim debt).
test "the parked-buffer gate, dead victim: three writers whose cold flushes fail retry once per tick, never spin on one another; every write completes when the device heals" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, false);
    defer w.deinit(testing.allocator);
    const waits0 = stats.parked_gate_waits.load();
    const cold0 = stats.parked_gate_cold_flushes.load();
    const fallbacks0 = accumulate.stats.write_through_fallbacks.load();
    for (0..8) |b| try parkHalf(w, io, 1, @intCast(b), @intCast(b + 1));
    // Every DMA fails until the test heals the device.
    w.f.dev.setFailWrites(std.math.maxInt(u32), error.InputOutput);
    const Writer = struct {
        w: *World,
        block: u32,
        result: anyerror!void = error.NotRun,
        fn run(self: *@This()) void {
            self.result = parkHalf(self.w, testing.io, 1, self.block, 9);
        }
    };
    var writers = [_]Writer{ .{ .w = w, .block = 8 }, .{ .w = w, .block = 9 }, .{ .w = w, .block = 10 } };
    var threads: [3]std.Thread = undefined;
    for (&threads, &writers) |*t, *wr| t.* = try std.Thread.spawn(.{}, Writer.run, .{wr});
    // Every writer's first cold flush failed (the upload's fallback) and all
    // three sit registered on the capacity notify.
    while (accumulate.stats.write_through_fallbacks.load() < fallbacks0 + 3 or w.wp.table.capacity.waiters(io) < 3) std.Thread.yield() catch {};
    const waits_at_park = stats.parked_gate_waits.load();
    const fallbacks_at_park = accumulate.stats.write_through_fallbacks.load();
    // They stay parked: thousands of yields later not one more flush was
    // attempted (the settle-riding gate attempted 130 in the same span).
    for (0..4096) |_| std.Thread.yield() catch {};
    try testing.expectEqual(fallbacks_at_park, accumulate.stats.write_through_fallbacks.load());
    try testing.expectEqual(waits_at_park, stats.parked_gate_waits.load());
    w.f.dev.setFailWrites(0, error.InputOutput);
    for (threads) |t| t.join();
    for (writers) |wr| try wr.result;
    // A round or two per writer after the heal (the tick, then a retire's
    // wake), never thousands.
    try testing.expect(stats.parked_gate_waits.load() - waits_at_park <= 9);
    try testing.expect(stats.parked_gate_waits.load() - waits0 >= 6);
    try testing.expect(stats.parked_gate_cold_flushes.load() - cold0 >= 4);
    try testing.expectEqual(@as(u32, 8), w.wp.table.parkedCount());
    for ([_]u32{ 8, 9, 10 }) |b| try testing.expect(w.wp.table.has(1, b));
}

// The wall: a plane that refuses the gate's cold flushes for the whole wall
// with no capacity returned answers the write ENOSPC, once and loud — the
// plane is broken, not slow; RAM is not grown past the cap and nothing is
// lost (the parked set stays).
test "the parked-buffer gate, the wall: cold flushes the plane keeps refusing end in ONE loud ENOSPC after the wall; the parked set is intact" {
    const io = testing.io;
    // The product wall is already shorter than one tick: the second
    // refused flush crosses it.
    const w = try World.init(testing.allocator, .sync_inline, false);
    defer w.deinit(testing.allocator);
    const refusals0 = stats.parked_gate_wall_refusals.load();
    for (0..8) |b| try parkHalf(w, io, 1, @intCast(b), @intCast(b + 1));
    w.f.dev.setFailWrites(std.math.maxInt(u32), error.InputOutput);
    log.testing.expectErrorLogs(1);
    var half: [wt.BS / 2]u8 align(4096) = undefined;
    fill(&half, 9);
    try testing.expectError(error.NoSpaceLeft, w.wp.write(io, .{ .ino = 1, .offset = 8 * wt.BS, .payload = .{ .bytes = &half }, .class = .entire_op, .file = World.striped(8 * wt.BS) }));
    try log.testing.expectAllFired();
    try testing.expectEqual(refusals0 + 1, stats.parked_gate_wall_refusals.load());
    try testing.expectEqual(@as(u32, 8), w.wp.table.parkedCount());
    try testing.expect(!w.wp.table.has(1, 8));
    w.f.dev.setFailWrites(0, error.InputOutput);
}

/// A device hook that holds every DMA until released (the window between a
/// DMA and its CQE, stretched); the upload task's thread yields meanwhile.
const HeldDma = struct {
    gate: std.atomic.Value(u32) = .init(0),
    hits: std.atomic.Value(u32) = .init(0),

    fn hook(self: *HeldDma) wt.Hook {
        return .{ .ctx = self, .call = &call };
    }

    fn call(ctx: *anyopaque, _: *wt.FakeDevice, _: u64, _: u64, _: u64) void {
        const self: *HeldDma = @ptrCast(@alignCast(ctx));
        _ = self.hits.fetchAdd(1, .acq_rel);
        while (self.gate.load(.acquire) == 0) std.Thread.yield() catch std.atomic.spinLoopHint();
    }

    fn release(self: *HeldDma) void {
        self.gate.store(1, .release);
    }
};

// The hot set: an upload in flight holds a cell; the writer at the cap
// waits for its settle (not a tick, not a cold flush) and proceeds when it
// lands.
test "the parked-buffer gate, hot set: a writer at the cap waits for the in-flight upload's settle and proceeds within the tick — no cold flush, no EAGAIN" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .adaptive, false);
    defer w.deinit(testing.allocator);
    var held: HeldDma = .{};
    w.f.dev.hook = held.hook();
    const waits0 = stats.parked_gate_waits.load();
    const cold0 = stats.parked_gate_cold_flushes.load();
    // One block completes and its detached upload blocks in the DMA (ino 2).
    try w.f.map.create(2, .striped, 0);
    var half: [wt.BS / 2]u8 align(4096) = undefined;
    fill(&half, 0x20);
    _ = try w.wp.write(io, .{ .ino = 2, .offset = 0, .payload = .{ .bytes = &half }, .class = .entire_op, .file = World.striped(0) });
    const o2 = try w.wp.write(io, .{ .ino = 2, .offset = wt.BS / 2, .payload = .{ .bytes = &half }, .class = .entire_op, .file = World.striped(wt.BS / 2) });
    try testing.expectEqual(@as(u32, 1), o2.count(.write_through));
    while (held.hits.load(.acquire) == 0) std.Thread.yield() catch {};
    // Seven parked partials take the other cells (ino 1, blocks 0..6).
    for (0..7) |b| try parkHalf(w, io, 1, @intCast(b), @intCast(b + 1));
    try testing.expect(!w.wp.table.hasRoom());
    // The eighth partial parks at the gate behind the upload.
    const Writer = struct {
        w: *World,
        result: anyerror!void = error.NotRun,
        fn run(self: *@This()) void {
            self.result = parkHalf(self.w, testing.io, 1, 7, 8);
        }
    };
    var writer: Writer = .{ .w = w };
    const t0 = clock.monotonicNs();
    const th = try std.Thread.spawn(.{}, Writer.run, .{&writer});
    while (w.wp.table.capacity.waiters(io) == 0) std.Thread.yield() catch {};
    // The DMA lands: the upload publishes, retires its entry (a cell
    // returns) and settles — the writer wakes and takes it.
    held.release();
    th.join();
    const waited = clock.monotonicNs() - t0;
    try writer.result;
    try testing.expect(waited < PARKED_COLD_NS);
    try testing.expect(stats.parked_gate_waits.load() > waits0);
    try testing.expectEqual(cold0, stats.parked_gate_cold_flushes.load());
    w.wp.quiesce(io);
    try testing.expect(w.f.map.seam().lookup(2, 0) != null);
    try testing.expect(w.wp.table.has(1, 7));
}

test "writeStatsJson aggregates every family under its module key" {
    var buf: [65536]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeStatsJson(&w);
    const s = w.buffered();
    try testing.expect(std.mem.startsWith(u8, s, "{\"admit_write_overlay_bytes\":"));
    inline for (.{ "\"overlay\":{", "\"patch\":{", "\"accumulate\":{", "\"extent\":{", "\"rewrite_epoch\":{", "\"pipeline\":{", "\"writeback\":{", "\"fsync\":{", "\"write_side_view\":{", "\"enospc\":{" }) |k| {
        try testing.expect(std.mem.indexOf(u8, s, k) != null);
    }
}

test {
    _ = @import("write_seams.zig");
    _ = @import("write_coverage.zig");
    _ = @import("coverage_model.zig");
    _ = @import("write_retry.zig");
    _ = @import("write_epoch.zig");
    if (builtin.os.tag == .linux) _ = @import("write_e2e_linux.zig");
}
