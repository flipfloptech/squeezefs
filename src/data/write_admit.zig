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
const sole_owner_patch = @import("sole_owner_patch.zig");
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
    parked_cap: u64,
    depth_override: pipeline.DepthOverride,
    fsync: fsync_mod.Levers,

    pub fn fromEnv(env: *const std.process.Environ.Map, block_size: u64, budget_bytes: u64) Config {
        const patch_max = sole_owner_patch.resolvePatchMax(knobs.optIntKnob("SQUEEZEFS_PATCH_MAX_BYTES", u64, env), block_size);
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
            .parked_cap = accumulate.parkedCapFromEnv(env, budget_bytes, block_size),
            .depth_override = pipeline.depthOverrideFromEnv(env),
            .fsync = fsync_mod.Levers.fromEnv(env),
        };
        cfg.logNonDefault(block_size);
        return cfg;
    }

    /// A/B levers announce themselves when off their shipped arm.
    fn logNonDefault(self: Config, block_size: u64) void {
        if (self.patch_max != sole_owner_patch.patchMaxDerived(block_size)) log.info("write path: A/B lever SQUEEZEFS_PATCH_MAX_BYTES={d} (derived {d})", .{ self.patch_max, sole_owner_patch.patchMaxDerived(block_size) });
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
    /// `parked_gate_self_flushes`: the W2 fold at the RAM bound (this
    /// writer's own deferred overlay folded in place, or escalated seeded).
    pub var parked_gate_self_flushes: counters.ShardedCounter = .{};
    /// `parked_gate_cold_flushes`: a writer at the cap flushed the coldest
    /// parked block (untouched ≥ `PARKED_COLD_NS`) to make its cell.
    pub var parked_gate_cold_flushes: counters.ShardedCounter = .{};
    /// `parked_gate_wall_refusals`: writes answered ENOSPC after
    /// `PARKED_GATE_WALL_NS` of plane-refused cold flushes with no capacity
    /// returned AND every data volume's fresh supply latched exhausted (the
    /// set is full). Must stay 0 on a set with free blocks.
    pub var parked_gate_wall_refusals: counters.ShardedCounter = .{};
    pub var write_shared_class_violation: counters.Tripwire = .init("write_shared_class_violation");
    /// The gate's cold flushes failed for a whole wall while fresh supply
    /// was VISIBLE on some volume: the plane is wrong, not full — the
    /// write stays parked (loud at the op watchdog's station), never
    /// ENOSPC (rung 9 F17: the holder's grant ledger refused every top-up
    /// at 25 % fill and 96 writes per leg were answered ENOSPC).
    pub var parked_gate_wall_supply_visible: counters.Tripwire = .init("parked_gate_wall_supply_visible");

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
    try w.print("\"write_shared_class_violation\":{d},\"parked_gate_wall_supply_visible\":{d},", .{ counters.tripwireCount("write_shared_class_violation"), counters.tripwireCount("parked_gate_wall_supply_visible") });
    try w.writeAll("\"overlay\":");
    try device_overlay.writeStatsJson(w);
    try w.writeAll(",\"patch\":");
    try sole_owner_patch.writeStatsJson(w);
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

/// The gate's station seam, on the writer's own fiber: `parked` runs once
/// per gate visit, before the wait (the FUSE handler stamps
/// `write_parked_pool` so the op watchdog names the park); `refused` runs
/// after a cold flush the plane REFUSED, with the writer still registered
/// on the capacity notify and its tick not yet started — the dead-victim
/// pin's rendezvous (a test holds the window there); null for the handler.
pub const ParkHook = struct {
    ctx: *anyopaque,
    parked: *const fn (ctx: *anyopaque) void,
    refused: ?*const fn (ctx: *anyopaque) void = null,
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
    /// The plaintext whole-block image of a binding (the plane decodes a
    /// transformed frame; null = raw device bytes, an all-passthrough set).
    image: ?seams.ImageSeam,

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
        /// The plaintext image seam (required when any volume is transformed).
        image: ?seams.ImageSeam = null,
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
            .image = d.image,
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
            .ram_custody = &ramCustody,
            .ram_custody_ctx = self,
        }, @intCast(cfg.parked_cap));
        self.view = .{ .table = &self.table, .overlay = &self.overlay, .epochs = &self.epochs, .locks = &self.locks, .device = d.device };
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
        return .{ .device = self.device, .map = self.map, .free = self.free, .purge = self.purge, .vols = self.vols, .picker = self.picker.picker(), .valve = self.valve, .epochs = &self.epochs, .stripes = self.stripes, .locks = &self.locks, .transform = self.transform, .scratch = self.scratch, .touched = &touchedStamp, .touched_ctx = self, .image = self.image, .custody = .{ .ctx = self, .ram_custody = &epochCustody, .overlay_open = &overlayOpen } };
    }

    fn touchedStamp(ctx: ?*anyopaque, ino: u64, ordinal: u32) void {
        const self: *WritePath = @ptrCast(@alignCast(ctx.?));
        self.touched.stampOrdinal(ino, ordinal);
    }

    pub fn patchDeps(self: *WritePath) sole_owner_patch.Deps {
        return .{ .device = self.device, .map = self.map, .vols = self.vols, .purge = self.purge, .custody = .{ .ctx = self, .ram_custody = &ramCustody, .overlay_open = &overlayOpen }, .touched = .{ .ctx = self, .stamp = &touchedStamp } };
    }

    /// The whole plaintext image of `b` (the seeded checkout's fill).
    fn imageRead(ctx: ?*anyopaque, b: Binding, out: []u8) seams.ReadError!void {
        const self: *WritePath = @ptrCast(@alignCast(ctx.?));
        if (self.image) |im| return im.read(im.ctx, b, out);
        return self.device.read(b.vol_tag, b.offset, out);
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
        begun: sole_owner_patch.Begun,
        len: u64,
    };

    pub const DirectVerdict = union(enum) {
        begun: DirectBegun,
        declined: DirectDecline,
    };

    /// The il direct-drive write gate (`il_daemon.md` §4.5): the lock-free
    /// shape half (`sole_owner_patch.shapeEligible` with the per-ino word READ, not
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
        if (!sole_owner_patch.shapeEligible(.{ .offset = offset, .len = len, .block_size = bs, .existing_size = file.size, .stream_adjacent = prev_end != 0 and prev_end == offset, .patch_max = self.cfg.patch_max })) return .{ .declined = .shape };
        const block: u32 = @intCast(offset / bs);
        const rel: u32 = @intCast(offset % bs);
        const guard = self.locks.tryAcquire(io, ino, block) orelse return .{ .declined = .block_lock };
        const begun = (sole_owner_patch.beginPatch(self.patchDeps(), ino, block, rel) catch |e| {
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

    pub const PatchError = sole_owner_patch.PatchError;

    /// A begun direct write the lane could not submit (no slab entry, a
    /// full SQ, an unknown volume): re-stabilize the word under a new
    /// generation (content unchanged, so no purge), hand the guard back;
    /// the caller takes the sever → handoff path.
    pub fn abortDirectWrite(self: *WritePath, io: Io, d: DirectBegun) void {
        sole_owner_patch.abandonInPlace(self.patchDeps(), d.begun);
        d.guard.release(io);
    }

    /// The CQE postlude: `finishPatch` (re-stabilize, purge, the touched
    /// stamp, the ledger — or the EIO law), then the guard drops.
    pub fn finishDirectWrite(self: *WritePath, io: Io, d: DirectBegun, dma: seams.DmaError!void) PatchError!void {
        defer d.guard.release(io);
        try sole_owner_patch.finishPatch(self.patchDeps(), d.begun, d.len, dma);
        stats.admit_write_inplace_bytes.add(d.len);
        stats.writes_admitted.inc();
    }

    fn ramCustody(ctx: ?*anyopaque, ino: u64, block: u32) bool {
        const self: *WritePath = @ptrCast(@alignCast(ctx.?));
        return self.table.has(ino, block) or self.epochs.bindsBlock(ino, block);
    }

    /// The in-place fold's RAM-custody probe: the folding entry IS the
    /// table's custody of the block, so only an epoch binding declines.
    fn epochCustody(ctx: ?*anyopaque, ino: u64, block: u32) bool {
        const self: *WritePath = @ptrCast(@alignCast(ctx.?));
        return self.epochs.bindsBlock(ino, block);
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
            log.err("write ino {d} [{d}, {d}): classified Shared against a {t} file of size {d}", .{ req.ino, req.offset, end, file.kind, file.size });
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
            sole_owner_patch.shapeEligible(.{ .offset = req.offset, .len = len, .block_size = bs, .existing_size = file.size, .stream_adjacent = stream_adjacent, .patch_max = self.cfg.patch_max });
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
    /// settled on the write arm's own ladder, the guard re-taken), when
    /// the overlay arm (B) meets a full write pipe (the guard is dropped,
    /// the governor's park runs holding no lock, the guard re-taken with
    /// the permit in hand) and when an accumulation arm meets the parked-
    /// buffer cap (the guard is dropped, the gate waits for a cell or
    /// makes one, the guard re-taken) — never a park under (3), never a
    /// refusal: a blocking `write(2)` on a regular file may not answer
    /// EAGAIN (POSIX reserves it for O_NONBLOCK; rung 9 F2 saw fio abort
    /// on 100–128 per leg). The governor's in-flight permits belong to
    /// uploads that take (3) to publish: a park for one under (3) is the
    /// generic/551 cycle.
    fn admitPiece(self: *WritePath, io: Io, req: Request, file: seams.FileView, pc: Piece, bytes: *?[]const u8, try_patch: bool, overlay_shape: bool, stream_adjacent: bool) Error!Arm {
        const bs = self.cfg.block_size;
        var hint: enum { none, overlay_drain, upload, fold } = .none;
        var upload_permit: ?pipeline.Permit = null;
        // The overlay arm's permit, admitted holding no guard; spent by the
        // store it is handed to, returned if another arm takes the piece.
        var overlay_permit: ?pipeline.Permit = null;
        defer if (overlay_permit) |p| self.governor.returnUnused(io, p);
        var prof = pipeline.Prof.start();
        var gate: GateWall = .{};
        const arm: Arm = blk: while (true) {
            const g = self.locks.acquire(io, req.ino, pc.block, .write_checkout);
            var held = true;
            defer if (held) g.release(io);
            prof.mark(.lock_wait);
            // A. W1.
            if (try_patch) {
                const vehicle: sole_owner_patch.Vehicle = if (bytes.*) |b| .{ .bytes = b[pc.off .. pc.off + pc.len] } else .{ .slot = req.payload.slot.src };
                if (try sole_owner_patch.tryPatch(self.patchDeps(), io, .{ .ino = req.ino, .block = pc.block, .rel = pc.rel, .vehicle = vehicle })) {
                    stats.admit_write_inplace_bytes.add(pc.len);
                    break :blk .patch;
                }
            }
            // B. Device overlay.
            if (overlay_shape and device_overlay.Overlay.pieceEligible(pc.rel, pc.len)) {
                const vehicle: device_overlay.Vehicle = if (bytes.*) |b| .{ .bytes = b[pc.off .. pc.off + pc.len] } else .{ .slot = req.payload.slot };
                const overwrite = self.map.lookup(req.ino, pc.block) != null;
                const handed = overlay_permit;
                overlay_permit = null;
                switch (try self.overlay.store(io, .{ .ino = req.ino, .block = pc.block, .rel = pc.rel, .vehicle = vehicle, .file = file, .overwrite = overwrite, .permit = handed })) {
                    .stored => |s| {
                        stats.admit_write_overlay_bytes.add(pc.len);
                        if (s.coverage_complete) hint = .overlay_drain;
                        break :blk .overlay;
                    },
                    .declined => {},
                    .governor_full => {
                        // The pipe is full: park for the permit holding
                        // no lock, then restart the ladder with it.
                        held = false;
                        g.release(io);
                        overlay_permit = self.governor.admitSegment(io, pc.len);
                        prof.mark(.admit_wait);
                        continue;
                    },
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
            // F. W2 extent park (§J.1): the page overlay. A small write into
            //    a mapped block on a TRANSFORMED volume cannot seed its page
            //    (a plaintext page costs a whole-frame decode): it takes the
            //    SEEDED checkout below (one decode, a content-valid full
            //    buffer that parks) instead of the overlay.
            const small = extent.isSmall(pc.len, bs);
            const existing = self.table.get(req.ino, pc.block);
            const base_passthrough = if (live_binding) |b| (if (self.vols.byTag(b.vol_tag)) |v| v.passthrough else false) else true;
            if (small and !stream_adjacent and (existing == null or existing.?.repr == .extent) and (existing != null or base_passthrough)) {
                const e = existing orelse self.table.checkoutExtent(req.ino, pc.block, has_existing, live_binding) orelse {
                    try self.parkAtPoolCap(io, g, &held, req, pc.block, &gate);
                    continue;
                };
                // No I/O: a hole's page is zeros, a deferred overlay's page
                // waits under its mask for the base (the fold's seed).
                e.repr.extent.merge(self.gpa, pc.rel, data) catch return error.OutOfMemory;
                e.write_epoch = accumulate.nextWriteEpoch();
                e.app_end = @max(e.app_end, pc.rel + pc.len);
                e.last_touch_ns = clock.monotonicNs();
                extent.stats.extent_parks.inc();
                stats.admit_write_accumulate_bytes.add(pc.len);
                const x = &e.repr.extent;
                if (x.isComplete(bs)) {
                    // Every page touched: the write-through trigger — the
                    // escalation completes the image (the base under a
                    // deferred overlay's masked bytes, zeros on a hole); one
                    // the pool cannot back now is DEFERRED to the next
                    // write/flush.
                    accumulate.escalateExtentLocked(&self.table, self.uploadDeps(), e) catch |err| switch (err) {
                        error.PoolExhausted => {
                            extent.stats.extent_escalations_deferred.inc();
                            break :blk .extent;
                        },
                        error.InputOutput, error.OutOfMemory => |err2| return err2,
                    };
                    hint = .upload;
                    break :blk .write_through;
                } else if (extent.foldDue(x.pageCount(), bs)) {
                    // The RAM bound: a hole block escalates HERE (zeros, no
                    // I/O) and parks until its one landing; a deferred
                    // overlay folds after the guard drops — in place at the
                    // page grain, or the CoW escalation seeded whole.
                    if (x.base_deferred and live_binding != null) {
                        hint = .fold;
                    } else if (self.table.escalate(e, null) == null) {
                        extent.stats.extent_escalations_deferred.inc();
                    }
                }
                break :blk .extent;
            }
            if (small and !stream_adjacent and existing == null and !base_passthrough) {
                const seeded = self.table.checkoutSeeded(req.ino, pc.block, live_binding.?, &imageRead, self) catch |err| switch (err) {
                    error.PoolExhausted => {
                        try self.parkAtPoolCap(io, g, &held, req, pc.block, &gate);
                        continue;
                    },
                    error.InputOutput => return error.InputOutput,
                };
                _ = self.table.recordWrite(seeded, pc.rel, data) catch |err| switch (err) {
                    error.PoolExhausted => {
                        try self.parkAtPoolCap(io, g, &held, req, pc.block, &gate);
                        continue;
                    },
                    error.NeedsSeed => unreachable, // seeded at checkout
                };
                stats.admit_write_accumulate_bytes.add(pc.len);
                break :blk .accumulate_park;
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
                // A larger or stream-adjacent piece on an extent entry: the
                // overlay escalates first (seeded whole from the base when
                // deferred — its untouched pages are the base's, never zeros).
                accumulate.escalateExtentLocked(&self.table, self.uploadDeps(), e) catch |err| switch (err) {
                    error.PoolExhausted => {
                        try self.parkAtPoolCap(io, g, &held, req, pc.block, &gate);
                        continue;
                    },
                    error.InputOutput, error.OutOfMemory => |x| return x,
                };
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
                // Detached like an upload (the bytes are acked and parked;
                // the fold is a flush): the triggering write never waits on
                // the fold's DMAs. Inline under the synchronous override.
                stats.parked_gate_self_flushes.inc();
                if (self.cfg.depth_override == .sync_inline) {
                    accumulate.foldExtentBlock(&self.table, self.uploadDeps(), io, req.ino, pc.block);
                } else self.tasks.spawn(io, foldTask, .{ self, io, req.ino, pc.block }) catch {
                    accumulate.foldExtentBlock(&self.table, self.uploadDeps(), io, req.ino, pc.block);
                };
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
                imageRead(self, old, img) catch return error.InputOutput;
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
    /// ENOSPC iff the SET is exhausted — every data volume's allocator
    /// latched `StorageFull` with no supply in sight (the charter's
    /// terminal law; `parked_gate_wall_refusals`, one loud line). With
    /// supply visible on any volume the plane is wrong, not full: the
    /// tripwire `parked_gate_wall_supply_visible` fires, the wall restarts
    /// and the write stays parked — never ENOSPC on a store with free
    /// blocks (rung 9 F17: the venue's aging store answered 96 per leg at
    /// 25 % fill when the holder's grant ledger refused every top-up).
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
                            var cands: [router.MAX_VOLUMES]*block_alloc.BlockAllocator = undefined;
                            if (enospc.freshSupplyExhausted(self.vols.candidates(&cands))) {
                                room.cancel(io);
                                stats.parked_gate_wall_refusals.inc();
                                log.err("write ino {d} block {d}: the parked-buffer gate's cold flushes have been refused for {d} ms (wall {d} ms; {d} parked, {d} B) and every data volume's fresh supply is latched exhausted — answering ENOSPC", .{ req.ino, block, (now - gate.failed_since_ns) / std.time.ns_per_ms, PARKED_GATE_WALL_NS / std.time.ns_per_ms, self.table.parkedCount(), accumulate.stats.parked_full_buffer_bytes.load() });
                                return error.NoSpaceLeft;
                            }
                            stats.parked_gate_wall_supply_visible.fire("the parked-buffer gate's cold flushes were refused for a whole wall while fresh supply was visible");
                            gate.failed_since_ns = now;
                        }
                        if (req.park_hook) |h| if (h.refused) |f| f(h.ctx);
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

    fn foldTask(self: *WritePath, io: Io, ino: u64, block: u32) Io.Cancelable!void {
        accumulate.foldExtentBlock(&self.table, self.uploadDeps(), io, ino, block);
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

    /// The pre-clip settle (a truncate, an O_TRUNC open, the corpse
    /// destroy): the ino's write-side custody lands or drops before its
    /// layout is read and clipped — exactly `flushIno` (a parked block's
    /// upload in flight is JOINED to its exit, tail included; the overlay
    /// drains; the epoch closes) and nothing process-wide. The old form
    /// added `quiesce`, a wait for EVERY ino's detached uploads through the
    /// one task set's single-awaiter group join: two truncates settling at
    /// once lost one wake for good (fstests generic/074 and 464 — ONE
    /// open(O_TRUNC) parked 35–40 min at `handler_entry`, no lock held,
    /// every thread asleep; `std.Io.Threaded` asserts on the same second
    /// awaiter).
    pub const settleCustody = flushIno;
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const wt = @import("write_testing.zig");
const sync_notify = @import("../sync/notify.zig");
const block_grant = @import("sym_planes/block_grant.zig");
const data_bitmap = @import("sym_planes/data_bitmap.zig");

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
    try testing.expectEqual(gauge_before - 2 * extent.PAGE_CHARGE, extent.stats.parked_extent_bytes.load()); // the pages left the extent budget for the buffer's
    // The shared-mode handoff: a Shared write that extends trips the wire
    // (the data plane still serves it) — the tripwire's line and the one
    // naming the ino, the range and the size.
    const viol0 = counters.tripwireCount("write_shared_class_violation");
    stats.write_shared_class_violation.resetLogBudgetForTest();
    log.testing.expectErrorLogs(2);
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

test "every acked byte reads back across the W2 overlay's escalation (repro-port: LTP fs gf27 — one-byte grow-by-lseek writes 4 KiB apart into block 1 read back 0 from the 65th on): 1-byte writes 512 B apart into one block, each served back by the write side before and after the RAM trigger; the flush lands all of them" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, false);
    defer w.deinit(testing.allocator);
    // Block 0 is mapped (the file's first block landed); block 1 is the hole
    // the grows extend into — the LTP shape past the first boundary.
    try w.f.map.create(30, .striped, 0);
    _ = try w.f.publishBlock(30, 0, 0xAA);
    const view = w.wp.writeSideView();
    const step: u64 = 512;
    const n_writes: u64 = wt.BS / step; // 128 writes over all 16 pages: past the quarter-block trigger
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
                var gaps = p.covered.gaps(0, wt.BS);
                while (gaps.next()) |gp| @memcpy(win[gp.start..gp.end], img[gp.start..gp.end]);
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

test "W2 amplification law, hole blocks (repro-port: fstests generic/074 `fstest -F -b 512` — 512 B fragmented writes extending a fresh file; 128× on c36b531): every block lands exactly ONCE at its flush — device bytes == blocks × block, zero seed reads, byte-exact readback (the patterns and the zeros between them)" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, false);
    defer w.deinit(testing.allocator);
    try w.f.map.create(40, .striped, 0);
    // 256 B every 512 B (a hole between every write), block after block:
    // 128 writes per 64 KiB test block, far past any count cap.
    const wlen: u64 = 256;
    const stride: u64 = 512;
    const n_blocks: u64 = 4;
    var buf: [256]u8 = undefined;
    var size: u64 = 0;
    var user: u64 = 0;
    var off: u64 = 0;
    const wt0 = accumulate.stats.write_through_blocks.load();
    while (off < n_blocks * wt.BS) : (off += stride) {
        fill(&buf, @truncate(off / stride));
        const o = try w.wp.write(io, .{ .ino = 40, .offset = off, .payload = .{ .bytes = &buf }, .class = .entire_op, .file = World.striped(size) });
        try testing.expectEqual(wlen, o.written);
        size = off + wlen;
        user += wlen;
    }
    w.wp.flushIno(io, 40);
    // The law: one whole-block landing per block, nothing read.
    try testing.expectEqual(n_blocks * wt.BS, w.f.dev.writeBytesTotal());
    try testing.expectEqual(@as(u64, 0), w.f.dev.reads);
    try testing.expectEqual(wt0 + n_blocks, accumulate.stats.write_through_blocks.load());
    try testing.expectEqual(extent.holeBoundPpm(wt.BS, wt.BS / 2), w.f.dev.writeBytesTotal() * 1_000_000 / user);
    // Byte-exact: every pattern where it was written, zeros in every hole.
    var got: [wt.BS]u8 = undefined;
    var b: u32 = 0;
    while (b < n_blocks) : (b += 1) {
        const m = w.f.map.seam().lookup(40, b) orelse return error.TestUnexpectedResult;
        w.f.dev.peek(m.vol_tag, m.offset, &got);
        var rel: u64 = 0;
        while (rel < wt.BS) : (rel += stride) {
            fill(&buf, @truncate((b * wt.BS + rel) / stride));
            try testing.expectEqualSlices(u8, &buf, got[rel .. rel + wlen]);
            try testing.expect(std.mem.allEqual(u8, got[rel + wlen .. rel + stride], 0));
        }
    }
}

test "W2 amplification law, a mapped sole-owner passthrough block folds IN PLACE at the page grain: 4 sector-aligned 512 B writes into 4 scattered pages land as 4 page DMAs (16 KiB for 2 KiB = the 8× bound, the seed exactly the touched pages, no allocation / merge / free, the binding unchanged); contiguous pages are ONE DMA; a lone parked write flushes as one page at release, never a whole-block upload" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, false);
    defer w.deinit(testing.allocator);
    const a = try w.f.publishBlock(41, 0, 0xAA);
    _ = try w.f.publishBlock(41, 1, 0xBB);
    _ = try w.f.publishBlock(41, 2, 0xCC);
    const file = World.striped(3 * wt.BS);
    const used = w.f.alloc_a.usedBlocks();
    const merges = w.f.map.merges.items.len;
    const ip0 = extent.stats.fold_inplace_write_bytes.load();
    const dmas0 = extent.stats.fold_inplace_dmas.load();
    const passes0 = extent.stats.fold_inplace_passes.load();
    const seeds0 = extent.stats.extent_page_seed_bytes.load();
    const folds0 = stats.parked_gate_self_flushes.load();
    var sector: [512]u8 = undefined;
    // Four scattered pages of block 0 (the 4th write is the quarter-block trigger at 16 pages).
    const pages = [_]u64{ 0, 2, 5, 9 };
    for (pages, 0..) |pg, i| {
        fill(&sector, @intCast(0x10 + i));
        const o = try w.wp.write(io, .{ .ino = 41, .offset = pg * PAGE + 1024, .payload = .{ .bytes = &sector }, .class = .shared, .file = file });
        try testing.expectEqual(@as(u32, 1), o.count(.extent));
    }
    try testing.expect(!w.wp.table.has(41, 0)); // folded and retired
    try testing.expectEqual(folds0 + 1, stats.parked_gate_self_flushes.load());
    try testing.expectEqual(passes0 + 1, extent.stats.fold_inplace_passes.load());
    try testing.expectEqual(dmas0 + 4, extent.stats.fold_inplace_dmas.load());
    try testing.expectEqual(ip0 + 4 * PAGE, extent.stats.fold_inplace_write_bytes.load());
    try testing.expectEqual(seeds0 + 4 * PAGE, extent.stats.extent_page_seed_bytes.load());
    try testing.expectEqual(@as(u64, 4 * PAGE), w.f.dev.writeBytesTotal());
    try testing.expectEqual(@as(u64, 4 * PAGE), w.f.dev.read_bytes);
    // The bound (tie): device ÷ user ≤ pages(512, 512) × P / 512 = 8×.
    try testing.expect(w.f.dev.writeBytesTotal() * 1_000_000 / (4 * 512) <= extent.inPlaceBoundPpm(512, 512));
    try testing.expectEqual(used, w.f.alloc_a.usedBlocks());
    try testing.expectEqual(merges, w.f.map.merges.items.len);
    try testing.expectEqual(@as(usize, 0), w.f.frees.freed.items.len);
    try testing.expect(w.f.map.seam().lookup(41, 0).?.eql(a));
    try testing.expect(block_alloc.Incarnation.isStable(w.f.alloc_a.incarnationWord(w.f.alloc_a.idxOf(a.offset))));
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(a.vol_tag, a.offset, &got);
    for (pages, 0..) |pg, i| {
        fill(&sector, @intCast(0x10 + i));
        try testing.expectEqualSlices(u8, &sector, got[pg * PAGE + 1024 .. pg * PAGE + 1536]);
        try testing.expect(std.mem.allEqual(u8, got[pg * PAGE .. pg * PAGE + 1024], 0xAA)); // the seeded rest of the page
        try testing.expect(std.mem.allEqual(u8, got[pg * PAGE + 1536 .. (pg + 1) * PAGE], 0xAA));
    }
    try testing.expect(std.mem.allEqual(u8, got[PAGE .. 2 * PAGE], 0xAA)); // an untouched page never written
    // Contiguous pages 10..13 of block 1: ONE DMA of 16 KiB.
    w.f.dev.resetLedger();
    const dmas1 = extent.stats.fold_inplace_dmas.load();
    var pg: u64 = 10;
    while (pg < 14) : (pg += 1) {
        fill(&sector, @intCast(pg));
        _ = try w.wp.write(io, .{ .ino = 41, .offset = wt.BS + pg * PAGE + 2048, .payload = .{ .bytes = &sector }, .class = .shared, .file = file });
    }
    try testing.expect(!w.wp.table.has(41, 1));
    try testing.expectEqual(dmas1 + 1, extent.stats.fold_inplace_dmas.load());
    try testing.expectEqual(@as(usize, 1), w.f.dev.writes.items.len);
    try testing.expectEqual(@as(u64, 4 * PAGE), w.f.dev.writes.items[0].len);
    // A lone write parks; RELEASE folds it in place as ONE page.
    w.f.dev.resetLedger();
    fill(&sector, 0x77);
    _ = try w.wp.write(io, .{ .ino = 41, .offset = 2 * wt.BS + 7 * PAGE, .payload = .{ .bytes = &sector }, .class = .shared, .file = file });
    try testing.expect(w.wp.table.has(41, 2));
    w.wp.flushIno(io, 41);
    try testing.expect(!w.wp.table.has(41, 2));
    try testing.expectEqual(@as(u64, PAGE), w.f.dev.writeBytesTotal());
    try testing.expectEqual(used, w.f.alloc_a.usedBlocks());
    try testing.expectEqual(merges, w.f.map.merges.items.len);
    const c = w.f.map.seam().lookup(41, 2).?;
    w.f.dev.peek(c.vol_tag, c.offset, &got);
    try testing.expectEqualSlices(u8, &sector, got[7 * PAGE .. 7 * PAGE + 512]);
    try testing.expect(std.mem.allEqual(u8, got[7 * PAGE + 512 .. 8 * PAGE], 0xCC));
    try testing.expectEqual(ip0 + 4 * PAGE + 4 * PAGE + PAGE, extent.stats.fold_inplace_write_bytes.load());
}

test "W2 amplification law, CoW bases land whole ONCE per parked episode: a cloned (pinned) block declines the in-place arm, escalates seeded (one whole read) and PARKS — later small writes are memcpys and the release uploads one block to a fresh destination; a transformed volume's block takes the seeded checkout (one decode) and likewise lands once" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, false);
    defer w.deinit(testing.allocator);
    const a = try w.f.publishBlock(42, 0, 0xAA);
    const file = World.striped(wt.BS);
    // A clone pins the block: refcount 2 ⇒ never in place.
    const idx = w.f.alloc_a.idxOf(a.offset);
    try testing.expectEqual(block_alloc.PinVerdict.pinned, w.f.alloc_a.pinBlockValidated(idx));
    const decl0 = extent.stats.fold_inplace_declines.load();
    const seeds0 = extent.stats.fold_seed_reads.load();
    const ip0 = extent.stats.fold_inplace_write_bytes.load();
    var sector: [512]u8 = undefined;
    var pg: u64 = 0;
    while (pg < 4) : (pg += 1) {
        fill(&sector, @intCast(pg));
        _ = try w.wp.write(io, .{ .ino = 42, .offset = pg * 3 * PAGE, .payload = .{ .bytes = &sector }, .class = .shared, .file = file });
    }
    try testing.expectEqual(decl0 + 1, extent.stats.fold_inplace_declines.load());
    try testing.expectEqual(seeds0 + 1, extent.stats.fold_seed_reads.load());
    try testing.expectEqual(ip0, extent.stats.fold_inplace_write_bytes.load());
    try testing.expectEqual(@as(u64, 0), w.f.dev.writeBytesTotal()); // parked, nothing landed
    const e = w.wp.table.get(42, 0).?;
    try testing.expect(e.repr == .full and e.content_valid);
    // The clone goes away; the parked full buffer still lands CoW (its
    // flush is an upload, never a patch).
    w.f.alloc_a.unpin(idx);
    // Eight more small writes: memcpys into the parked buffer.
    const reads_before = w.f.dev.read_bytes;
    pg = 0;
    while (pg < 8) : (pg += 1) {
        fill(&sector, @intCast(0x40 + pg));
        _ = try w.wp.write(io, .{ .ino = 42, .offset = pg * PAGE + 100, .payload = .{ .bytes = &sector }, .class = .shared, .file = file });
    }
    try testing.expectEqual(reads_before, w.f.dev.read_bytes);
    try testing.expectEqual(@as(u64, 0), w.f.dev.writeBytesTotal());
    w.wp.flushIno(io, 42);
    try testing.expectEqual(@as(u64, wt.BS), w.f.dev.writeBytesTotal()); // ONE landing
    const b = w.f.map.seam().lookup(42, 0).?;
    try testing.expect(!b.eql(a)); // CoW: a fresh destination
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(b.vol_tag, b.offset, &got);
    fill(&sector, 0x40);
    try testing.expectEqualSlices(u8, &sector, got[100..612]);
    fill(&sector, 1);
    try testing.expectEqualSlices(u8, sector[0..100], got[3 * PAGE .. 3 * PAGE + 100]); // the first write's head; the later write at +100 wins the rest
    try testing.expect(std.mem.allEqual(u8, got[PAGE + 612 .. 2 * PAGE], 0xAA));
    // The transformed volume (B is not passthrough): a mapped block there
    // takes the seeded checkout — one image read, a parked content-valid
    // buffer, one landing at release.
    const mb = try w.f.alloc_b.allocateBlock();
    var img: [wt.BS]u8 = @splat(0xB0);
    try w.f.dev.seam().vtable.write(&w.f.dev, wt.VOL_B, mb.offset, &img);
    w.f.alloc_b.publishBlock(mb.idx);
    const bb = Binding.wholeBlock(wt.VOL_B, mb.offset, mb.stamp);
    try w.f.map.create(43, .striped, wt.BS);
    try w.f.map.bind(43, 0, bb);
    w.f.dev.resetLedger();
    const mat0 = accumulate.stats.overwrite_seed_materialized.load();
    fill(&sector, 0x55);
    const o = try w.wp.write(io, .{ .ino = 43, .offset = 5 * PAGE + 7, .payload = .{ .bytes = &sector }, .class = .shared, .file = file });
    try testing.expectEqual(@as(u32, 1), o.count(.accumulate_park));
    try testing.expectEqual(mat0 + 1, accumulate.stats.overwrite_seed_materialized.load());
    try testing.expectEqual(@as(u64, wt.BS), w.f.dev.read_bytes);
    _ = try w.wp.write(io, .{ .ino = 43, .offset = 9 * PAGE + 7, .payload = .{ .bytes = &sector }, .class = .shared, .file = file });
    try testing.expectEqual(@as(u64, wt.BS), w.f.dev.read_bytes); // no second read
    try testing.expectEqual(@as(u64, 0), w.f.dev.writeBytesTotal());
    w.wp.flushIno(io, 43);
    try testing.expectEqual(@as(u64, wt.BS), w.f.dev.writeBytesTotal());
    const nb = w.f.map.seam().lookup(43, 0).?;
    w.f.dev.peek(nb.vol_tag, nb.offset, &got);
    try testing.expectEqualSlices(u8, &sector, got[5 * PAGE + 7 .. 5 * PAGE + 519]);
    try testing.expectEqualSlices(u8, &sector, got[9 * PAGE + 7 .. 9 * PAGE + 519]);
    try testing.expect(std.mem.allEqual(u8, got[0 .. 5 * PAGE + 7], 0xB0));
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
    // dd #N: an unaligned 4 KiB overwrite of block 1 (W1-ineligible ⇒ W2
    // park). A1 is CLONED (pinned: refcount 2) so the flush takes the CoW
    // arm — a sole-owner block would fold in place and open no epoch.
    const a1_idx = w.f.alloc_a.idxOf(a1.offset);
    try testing.expectEqual(block_alloc.PinVerdict.pinned, w.f.alloc_a.pinBlockValidated(a1_idx));
    var p: [PAGE]u8 = @splat(0x11);
    const o1 = try w.wp.write(io, .{ .ino = 50, .offset = wt.BS + 3 * PAGE + 100, .payload = .{ .bytes = &p }, .class = .shared, .file = file });
    try testing.expectEqual(@as(u32, 1), o1.count(.extent));
    // RELEASE #N, first half: the parked block flushes and FEEDS the epoch
    // (shadow B over block 1, A1 parked); the ONE save has not run yet.
    try testing.expectEqual(accumulate.Disposition.published, accumulate.flushParkedBlock(&w.wp.table, w.wp.uploadDeps(), io, 50, 1));
    w.f.alloc_a.unpin(a1_idx);
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
    // RELEASE #N+1: B is the sole-owner map key now — the parked page
    // folds IN PLACE into B (its untouched bytes were seeded from B at the
    // touch), nothing is displaced.
    w.wp.flushIno(io, 50);
    const final = w.f.map.seam().lookup(50, 1).?;
    try testing.expect(final.eql(b));
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(final.vol_tag, final.offset, &got);
    try testing.expect(std.mem.allEqual(u8, got[3 * PAGE + 100 .. 4 * PAGE + 100], 0x11)); // dd #N's bytes
    try testing.expect(std.mem.allEqual(u8, got[8 * PAGE .. 9 * PAGE], 0x22)); // dd #N+1's bytes
    try testing.expect(std.mem.allEqual(u8, got[0 .. 3 * PAGE], 0xBB));
    try testing.expect(std.mem.allEqual(u8, got[9 * PAGE ..], 0xBB));
    try testing.expectEqual(@as(usize, 1), w.f.frees.freed.items.len);
    try testing.expect(w.f.frees.freed.items[0].eql(a1));
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
    return parkHalfHooked(w, io, ino, block, seed, null);
}

/// `parkHalf` with the gate's station seam attached.
fn parkHalfHooked(w: *World, io: Io, ino: u64, block: u32, seed: u8, hook: ?ParkHook) !void {
    var half: [wt.BS / 2]u8 align(4096) = undefined;
    fill(&half, seed);
    if (w.f.map.files.get(ino) == null) try w.f.map.create(ino, .striped, 0);
    const o = try w.wp.write(io, .{ .ino = ino, .offset = @as(u64, block) * wt.BS, .payload = .{ .bytes = &half }, .class = .entire_op, .file = World.striped(@as(u64, block) * wt.BS), .park_hook = hook });
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

/// The dead-victim pin's rendezvous on the gate's `refused` station: a
/// writer whose cold flush the plane refused counts itself in and HOLDS
/// there — registered on the capacity notify, its tick not started — until
/// the test releases the set; a finished writer counts itself out. Both
/// sides are enable-then-recheck over `sync.Notify`; the test's waits carry
/// a deadline and fail loudly, never wedge.
const RefusedHold = struct {
    /// Writers at the station (release; the test's acquire load pairs).
    reached: Atomic(u32) = .init(0),
    /// Writers whose write returned (release; the test's acquire pairs).
    done: Atomic(u32) = .init(0),
    /// Bumped on every count: the test re-checks its own predicate.
    counted: sync_notify.Notify = .init,
    /// Set once by the test (release; the writers' acquire load pairs).
    released: Atomic(u32) = .init(0),
    release_notify: sync_notify.Notify = .init,

    fn hook(self: *RefusedHold) ParkHook {
        return .{ .ctx = self, .parked = &noStation, .refused = &refused };
    }

    fn noStation(_: *anyopaque) void {}

    fn refused(ctx: *anyopaque) void {
        const self: *RefusedHold = @ptrCast(@alignCast(ctx));
        const io = testing.io;
        self.count(io, &self.reached);
        while (true) {
            var n = self.release_notify.notified();
            _ = n.enable(io);
            if (self.released.load(.acquire) != 0) {
                n.cancel(io);
                return;
            }
            n.waitUncancelable(io);
        }
    }

    fn count(self: *RefusedHold, io: Io, word: *Atomic(u32)) void {
        _ = word.fetchAdd(1, .release);
        self.counted.notifyWaiters(io);
    }

    fn release(self: *RefusedHold, io: Io) void {
        self.released.store(1, .release);
        self.release_notify.notifyWaiters(io);
    }

    /// `word >= want` before the deadline, else `error.RendezvousDeadline`.
    fn awaitCount(self: *RefusedHold, io: Io, word: *const Atomic(u32), want: u32, deadline_ns: u64) error{RendezvousDeadline}!void {
        while (true) {
            var n = self.counted.notified();
            _ = n.enable(io);
            if (word.load(.acquire) >= want) {
                n.cancel(io);
                return;
            }
            if (n.waitUntilUncancelable(io, deadline_ns) == .deadline) {
                if (word.load(.acquire) >= want) return;
                return error.RendezvousDeadline;
            }
        }
    }
};

/// The dead-victim pin's bound on every rendezvous: a writer reaches the
/// station in one ladder round and a refused fake DMA (microseconds), and
/// completes one tick after the heal; two ticks past either, the gate is
/// wedged and the row goes red with its counters instead of hanging.
const DEAD_VICTIM_BOUND_NS: u64 = 2 * PARKED_COLD_NS;
/// The quiet window after the F2 trigger: a quarter tick — inside the
/// ticks that began at the release by a wide margin, and ~100 round trips
/// of a settle-riding gate (the pristine shape re-flushed 130 times in a
/// shorter span).
const DEAD_VICTIM_QUIET_NS: u64 = PARKED_COLD_NS / 4;

// A dead victim: the coldest block's flush fails (no fresh block, a failed
// DMA) — the gate waits a whole tick before trying again, woken only by a
// RETIRE (capacity returned), never by an upload's exit: a failed upload
// settles `upload_settled` too, and three writers at the cap riding that
// notify woke one another's failed flushes forever (the bracket harness's
// seq-write-1m-buffered leg: 7.7 M, then 3.75 M gate rounds in four
// minutes on a set whose free blocks all sat in reclaim debt).
//
// The pin rides the gate's `refused` station (`RefusedHold`): the arrivals
// are serialized there — a writer entering while a sibling's flush is in
// flight DEFERS to that upload instead of flushing (the hot-set law), which
// made the pristine pin's "three first-round flushes" a schedule lottery
// whose late flushes landed on the first writer's tick (`expected 3, found
// 4`, the wall's tripwire, and a World torn down under three live writers:
// SIGSEGV, or the wedge of 2026-10-10). Held, nobody's tick runs while the
// others arrive; released together, the F2 trigger is fired at the parked
// set from the test thread — a refused flush's settle — and the quiet
// window is a deadline wait on `upload_settled` itself: a settle inside it
// IS a writer re-flushing on a settle.
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
    var hold: RefusedHold = .{};
    const Writer = struct {
        w: *World,
        hold: *RefusedHold,
        block: u32,
        result: anyerror!void = error.NotRun,
        fn run(self: *@This()) void {
            self.result = parkHalfHooked(self.w, testing.io, 1, self.block, 9, self.hold.hook());
            self.hold.count(testing.io, &self.hold.done);
        }
    };
    var writers = [_]Writer{ .{ .w = w, .hold = &hold, .block = 8 }, .{ .w = w, .hold = &hold, .block = 9 }, .{ .w = w, .hold = &hold, .block = 10 } };
    var threads: [3]std.Thread = undefined;
    var spawned: usize = 0;
    var joined = false;
    // A red row never tears the World down under live writers: release the
    // held ones, fail the rest out of the gate (`VolumeFailed` ⇒
    // `.fence_drop` ⇒ the write errors at its next tick) and join.
    defer if (!joined) {
        hold.release(io);
        w.f.dev.setFailWrites(std.math.maxInt(u32), error.VolumeFailed);
        for (threads[0..spawned]) |t| t.join();
    };
    const Loud = struct {
        fn counters(w_: *World, h: *RefusedHold, io_: Io, waits0_: u64, cold0_: u64, fallbacks0_: u64) void {
            std.debug.print("dead victim: gate waits +{d}, cold flushes +{d}, fallbacks +{d}, capacity waiters {d}, at the station {d}, done {d}\n", .{ stats.parked_gate_waits.load() - waits0_, stats.parked_gate_cold_flushes.load() - cold0_, accumulate.stats.write_through_fallbacks.load() - fallbacks0_, w_.wp.table.capacity.waiters(io_), h.reached.load(.acquire), h.done.load(.acquire) });
        }
    };
    // The arrivals, one at a time: writer k is spawned once writer k−1 holds
    // at the station (its refused flush settled, no upload in flight), so
    // each one's FIRST round is a cold flush, never the hot-set deferral.
    const arrival_deadline = clock.monotonicNs() + DEAD_VICTIM_BOUND_NS;
    for (&threads, &writers, 1..) |*t, *wr, k| {
        t.* = try std.Thread.spawn(.{}, Writer.run, .{wr});
        spawned += 1;
        hold.awaitCount(io, &hold.reached, @intCast(k), arrival_deadline) catch |e| {
            std.debug.print("dead victim: writer {d} never reached the refused station\n", .{k});
            Loud.counters(w, &hold, io, waits0, cold0, fallbacks0);
            return e;
        };
    }
    // Exactly one gate visit and one refused cold flush per writer; all
    // three registered on the capacity notify, nothing in flight.
    try testing.expectEqual(waits0 + 3, stats.parked_gate_waits.load());
    try testing.expectEqual(cold0 + 3, stats.parked_gate_cold_flushes.load());
    try testing.expectEqual(fallbacks0 + 3, accumulate.stats.write_through_fallbacks.load());
    try testing.expectEqual(@as(u32, 3), w.wp.table.capacity.waiters(io));
    try testing.expect(!w.wp.table.anyUploadInflight());
    // Released together: three ticks start now.
    hold.release(io);
    // The F2 trigger: a refused flush's settle fired at three writers
    // parked at the gate (block 3 stays parked, zero-completed).
    try testing.expectEqual(accumulate.Disposition.stay_parked, accumulate.flushParkedBlock(&w.wp.table, w.wp.uploadDeps(), io, 1, 3));
    try testing.expectEqual(fallbacks0 + 4, accumulate.stats.write_through_fallbacks.load());
    // The quiet window on the settle notify itself.
    var settle = w.wp.table.upload_settled.notified();
    _ = settle.enable(io);
    const quiet = settle.waitUntilUncancelable(io, clock.monotonicNs() + DEAD_VICTIM_QUIET_NS);
    if (quiet != .deadline or stats.parked_gate_waits.load() != waits0 + 3 or accumulate.stats.write_through_fallbacks.load() != fallbacks0 + 4) {
        std.debug.print("dead victim: a writer re-flushed inside its tick (settle wait: {t})\n", .{quiet});
        Loud.counters(w, &hold, io, waits0, cold0, fallbacks0);
        return error.WriterSpunOnSettle;
    }
    // The heal: the ticks' flushes land and every write completes — a round
    // or two per writer (the tick, then a retire's wake), never thousands.
    w.f.dev.setFailWrites(0, error.InputOutput);
    hold.awaitCount(io, &hold.done, 3, clock.monotonicNs() + DEAD_VICTIM_BOUND_NS) catch |e| {
        std.debug.print("dead victim: the writes did not complete within two ticks of the heal\n", .{});
        Loud.counters(w, &hold, io, waits0, cold0, fallbacks0);
        return e;
    };
    for (threads) |t| t.join();
    joined = true;
    for (writers) |wr| try wr.result;
    try testing.expect(stats.parked_gate_waits.load() - (waits0 + 3) <= 9);
    try testing.expect(stats.parked_gate_waits.load() - waits0 >= 6);
    try testing.expect(stats.parked_gate_cold_flushes.load() - cold0 >= 4);
    try testing.expectEqual(@as(u32, 8), w.wp.table.parkedCount());
    try testing.expect(w.wp.table.has(1, 3));
    for ([_]u32{ 8, 9, 10 }) |b| try testing.expect(w.wp.table.has(1, b));
}

// The wall: a plane that refuses the gate's cold flushes for the whole wall
// with no capacity returned answers the write ENOSPC, once and loud, iff the
// SET is exhausted (every volume's allocator latched, nothing in sight) —
// RAM is not grown past the cap and nothing is lost (the parked set stays).
test "the parked-buffer gate, the wall on an EXHAUSTED set: cold flushes the full set keeps refusing end in ONE loud ENOSPC after the wall; the parked set is intact" {
    const io = testing.io;
    // The product wall is already shorter than one tick: the second
    // refused flush crosses it.
    const w = try World.init(testing.allocator, .sync_inline, false);
    defer w.deinit(testing.allocator);
    const refusals0 = stats.parked_gate_wall_refusals.load();
    const visible0 = counters.tripwireCount("parked_gate_wall_supply_visible");
    const admission0 = stats.write_enospc_refusals.load();
    for (0..8) |b| try parkHalf(w, io, 1, @intCast(b), @intCast(b + 1));
    // A reader's snapshot shares block 7's cell: the sibling piece below
    // must copy away into a fresh cell at the cap — the gate, reached PAST
    // the admission's terminal law (custody held, never fresh).
    var snap = w.wp.table.snapshot(w.wp.table.get(1, 7).?);
    defer snap.release(&w.wp.table.pool);
    // Every block of both volumes minted away: the set is full for good.
    while (w.f.alloc_a.allocateBlock()) |_| {} else |_| {}
    while (w.f.alloc_b.allocateBlock()) |_| {} else |_| {}
    var cands: [router.MAX_VOLUMES]*block_alloc.BlockAllocator = undefined;
    try testing.expect(enospc.freshSupplyExhausted(w.f.vols.candidates(&cands)));
    log.testing.expectErrorLogs(1);
    var half: [wt.BS / 2]u8 align(4096) = undefined;
    fill(&half, 9);
    try testing.expectError(error.NoSpaceLeft, w.wp.write(io, .{ .ino = 1, .offset = 7 * wt.BS + wt.BS / 2, .payload = .{ .bytes = &half }, .class = .entire_op, .file = World.striped(7 * wt.BS + wt.BS / 2) }));
    try log.testing.expectAllFired();
    try testing.expectEqual(admission0, stats.write_enospc_refusals.load()); // the gate's wall, not the admission
    try testing.expectEqual(refusals0 + 1, stats.parked_gate_wall_refusals.load());
    try testing.expectEqual(visible0, counters.tripwireCount("parked_gate_wall_supply_visible"));
    try testing.expectEqual(@as(u32, 8), w.wp.table.parkedCount());
    try testing.expect(w.wp.table.has(1, 7)); // the first half stays parked (never a lost byte)
}

// Rung 9 F17: the venue's aging store answered ENOSPC 96 times per leg at
// 25 % fill — the gate's wall fired on a plane that was WRONG (the holder's
// grant ledger refused every top-up), not FULL. With fresh supply visible
// on any volume the wall is a tripwire and a restart, never a refusal: the
// write stays parked and completes when the plane heals.
test "the parked-buffer gate, the wall with supply VISIBLE: a plane refusing the cold flushes past the wall fires `parked_gate_wall_supply_visible`, answers no ENOSPC, and the write completes when the plane heals" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, false);
    defer w.deinit(testing.allocator);
    const refusals0 = stats.parked_gate_wall_refusals.load();
    const visible0 = counters.tripwireCount("parked_gate_wall_supply_visible");
    for (0..8) |b| try parkHalf(w, io, 1, @intCast(b), @intCast(b + 1));
    var cands: [router.MAX_VOLUMES]*block_alloc.BlockAllocator = undefined;
    try testing.expect(!enospc.freshSupplyExhausted(w.f.vols.candidates(&cands)));
    // Every DMA fails until the test heals the device: the flushes are
    // refused, the mints abandoned — the free blocks stay in sight.
    w.f.dev.setFailWrites(std.math.maxInt(u32), error.InputOutput);
    // The log seam is per thread: the writer's thread declares the ONE
    // loud line (the tripwire's) and checks it fired.
    const Writer = struct {
        w: *World,
        result: anyerror!void = error.NotRun,
        fired: anyerror!void = error.NotRun,
        fn run(self: *@This()) void {
            log.testing.expectErrorLogs(1);
            const r = parkHalf(self.w, testing.io, 1, 8, 9);
            self.fired = log.testing.expectAllFired();
            self.result = r;
        }
    };
    var wr: Writer = .{ .w = w };
    stats.parked_gate_wall_supply_visible.resetLogBudgetForTest();
    const t = try std.Thread.spawn(.{}, Writer.run, .{&wr});
    // Past the wall: the tripwire fired, no refusal, the write still parked.
    while (counters.tripwireCount("parked_gate_wall_supply_visible") == visible0) std.Thread.yield() catch {};
    try testing.expectEqual(refusals0, stats.parked_gate_wall_refusals.load());
    try testing.expect(!w.wp.table.has(1, 8)); // still at the gate: no byte of it recorded
    w.f.dev.setFailWrites(0, error.InputOutput);
    t.join();
    try wr.result;
    try wr.fired;
    try testing.expectEqual(refusals0, stats.parked_gate_wall_refusals.load());
    try testing.expectEqual(@as(u32, 8), w.wp.table.parkedCount());
    try testing.expect(w.wp.table.has(1, 8));
}

/// A data volume of the test World armed as the grant plane's HOLDER: the
/// bitmap over the allocator (its finish-free sink), a small ledger, the
/// own window bound to its sink — the solo symmetric mount's shape.
const ArmedVolume = struct {
    bitmap: *data_bitmap.DataBitmap,
    holder: block_grant.HolderGrants,
    arm: block_grant.WriterArm,
    sink: block_grant.LocalSink,

    fn arm_(self: *ArmedVolume, gpa: Allocator, alloc: *block_alloc.BlockAllocator, ledger_slots: usize) !void {
        const blocks = alloc.capacityBlocks();
        self.bitmap = try data_bitmap.DataBitmap.init(gpa, alloc, .{ .vol_tag = alloc.volTag(), .term = 1, .blocks = blocks, .ring_capacity = data_bitmap.ringCapacity(block_grant.BLOCK_GRANT_FLOOR, 1, 64) });
        self.holder = .{ .ledger = try block_grant.Ledger.init(gpa, ledger_slots), .bitmap = self.bitmap };
        self.arm = .{ .capacity_blocks = blocks };
        self.sink = .{ .holder = &self.holder, .writer = 0, .window = &self.arm.window };
        self.arm.sink = self.sink.sink();
        alloc.grant_source = self.arm.grantSource();
        alloc.finish_free_sink = self.bitmap.finishFreeSink();
    }

    fn disarm(self: *ArmedVolume, alloc: *block_alloc.BlockAllocator) void {
        alloc.grant_source = null;
        alloc.finish_free_sink = null;
        self.holder.ledger.deinit();
        self.bitmap.deinit();
    }
};

// Rung 9 F17 through the write path: the bracket harness's aging store in
// miniature — two grant-armed volumes, four files of four blocks (a quarter
// of the set live), whole-block overwrites lap after lap through the overlay
// and the accumulation arms, every displaced block freed back into the
// holder's bitmap. The ledger has 16 slots (the product's 1,024 filled in
// one 60 s leg the same way once the carves fragmented): at `53d05e1` the
// consumed grants filled it, every top-up answered `deferred`, the mints
// read `StorageFull` at a quarter full, the pieces parked, the pool filled
// and the gate's wall answered ENOSPC (96 per leg).
test "the aging store through the write path (rung 9 F17): grant-armed volumes under laps of whole-block overwrites — every write lands, no ENOSPC, the gate's wall never fires, the ledger never fills, the set never reads exhausted" {
    const io = testing.io;
    const gpa = testing.allocator;
    const w = try World.init(gpa, .sync_inline, true);
    defer w.deinit(gpa);
    var va: ArmedVolume = undefined;
    try va.arm_(gpa, w.f.alloc_a, 16);
    defer va.disarm(w.f.alloc_a);
    var vb: ArmedVolume = undefined;
    try vb.arm_(gpa, w.f.alloc_b, 16);
    defer vb.disarm(w.f.alloc_b);
    const wall0 = stats.parked_gate_wall_refusals.load();
    const visible0 = counters.tripwireCount("parked_gate_wall_supply_visible");
    const refusals0 = stats.write_enospc_refusals.load();
    const deferred0 = block_grant.stats.block_grant_deferred.load();
    const exhausted0 = block_grant.stats.block_grant_exhausted.load();
    const files: u64 = 4;
    const blocks_per_file: u32 = 4;
    var whole: [wt.BS]u8 align(4096) = undefined;
    var lap: u8 = 0;
    while (lap < 12) : (lap += 1) {
        var ino: u64 = 1;
        while (ino <= files) : (ino += 1) {
            if (lap == 0) try w.f.map.create(ino, .striped, 0);
            var b: u32 = 0;
            while (b < blocks_per_file) : (b += 1) {
                // The laps interleave the files' blocks so the frees scatter.
                const block = (b + lap) % blocks_per_file;
                fill(&whole, @truncate(lap * 17 + block));
                const size: u64 = if (lap == 0) @as(u64, block) * wt.BS else blocks_per_file * wt.BS;
                const o = w.wp.write(io, .{ .ino = ino, .offset = @as(u64, block) * wt.BS, .payload = .{ .bytes = &whole }, .class = .entire_op, .file = World.striped(size) }) catch |e| {
                    std.debug.print("lap {d} ino {d} block {d}: {t}; clear bits a {d} b {d}, open grants a {d} b {d}, parked {d}\n", .{ lap, ino, block, e, w.f.alloc_a.capacityBlocks() - va.bitmap.populationNow(), w.f.alloc_b.capacityBlocks() - vb.bitmap.populationNow(), va.holder.ledger.openCount(), vb.holder.ledger.openCount(), w.wp.table.parkedCount() });
                    return e;
                };
                try testing.expectEqual(wt.BS, o.written);
            }
            w.wp.quiesce(io);
        }
        // Every file's epoch closed (full coverage): the displaced blocks are free again.
        ino = 1;
        while (ino <= files) : (ino += 1) w.wp.flushIno(io, ino);
        w.wp.quiesce(io);
    }
    try testing.expectEqual(wall0, stats.parked_gate_wall_refusals.load());
    try testing.expectEqual(visible0, counters.tripwireCount("parked_gate_wall_supply_visible"));
    try testing.expectEqual(refusals0, stats.write_enospc_refusals.load());
    try testing.expectEqual(deferred0, block_grant.stats.block_grant_deferred.load());
    try testing.expectEqual(exhausted0, block_grant.stats.block_grant_exhausted.load());
    try testing.expect(va.holder.ledger.openCount() <= block_grant.GRANTS_MAX + 1);
    try testing.expect(vb.holder.ledger.openCount() <= block_grant.GRANTS_MAX + 1);
    var cands: [router.MAX_VOLUMES]*block_alloc.BlockAllocator = undefined;
    try testing.expect(!enospc.freshSupplyExhausted(w.f.vols.candidates(&cands)));
    // The live set is what the files hold: nothing parked, nothing leaked.
    try testing.expectEqual(@as(u32, 0), w.wp.table.parkedCount());
    // The last lap's bytes are on the device, byte-exact.
    var got: [wt.BS]u8 = undefined;
    const last_lap: u8 = 11;
    var ino: u64 = 1;
    while (ino <= files) : (ino += 1) {
        var b: u32 = 0;
        while (b < blocks_per_file) : (b += 1) {
            const m = w.f.map.seam().lookup(ino, b) orelse return error.BlockUnmapped;
            w.f.dev.peek(m.vol_tag, m.offset, &got);
            fill(&whole, @truncate(last_lap * 17 + b));
            try testing.expectEqualSlices(u8, &whole, &got);
        }
    }
}

/// A device hook that holds every DMA until released (the window between a
/// DMA and its CQE, stretched); the upload task's thread yields meanwhile.
const HeldDma = struct {
    gate: std.atomic.Value(u32) = .init(0),
    hits: std.atomic.Value(u32) = .init(0),
    /// Hold only the FIRST DMA; later ones pass (a held upload of one ino
    /// beside other inos' flushes).
    first_only: bool = false,

    fn hook(self: *HeldDma) wt.Hook {
        return .{ .ctx = self, .call = &call };
    }

    fn call(ctx: *anyopaque, _: *wt.FakeDevice, _: u64, _: u64, _: u64) void {
        const self: *HeldDma = @ptrCast(@alignCast(ctx));
        const nth = self.hits.fetchAdd(1, .acq_rel);
        if (self.first_only and nth != 0) return;
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

// fstests generic/551 (`aio-dio-write-verify`: 53 concurrent 512 B-aligned
// O_DIRECT AIO writes to one file): the kernel splits each write into a
// page-aligned middle and unaligned ends; a block whose unaligned end had
// already parked took its aligned middle through the OVERLAY — two
// write-side authorities on one block, each publishing its complement
// (zeros: the block was a hole) over the other's bytes. The verify read a
// 1 MiB run of zeros where every writer had put 0x5a.
test "the overlay never opens a second authority on a block the RAM table holds: an aligned piece arriving at a parked block takes the accumulation arm, and after the flush every byte of both pieces is on the device" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, true);
    defer w.deinit(testing.allocator);
    try w.f.map.create(1, .striped, 0);
    // The unaligned head parks (a fresh entry whose complement is zeros).
    var head: [3 * PAGE + 100]u8 = undefined;
    fill(&head, 0x51);
    const o1 = try w.wp.write(io, .{ .ino = 1, .offset = 0, .payload = .{ .bytes = &head }, .class = .entire_op, .file = World.striped(0) });
    try testing.expectEqual(@as(u32, 1), o1.count(.accumulate_park) + o1.count(.extent));
    try testing.expect(w.wp.table.has(1, 0));
    // The aligned middle is overlay-shaped (page-aligned, above the W1
    // ceiling, a fresh block) — but the block is HELD.
    const installs0 = device_overlay.stats.overlay_installs.load();
    const declines0 = device_overlay.stats.overlay_ineligible_ram_custody.load();
    var mid: [4 * PAGE]u8 align(4096) = undefined;
    fill(&mid, 0x52);
    const o2 = try w.wp.write(io, .{ .ino = 1, .offset = 8 * PAGE, .payload = .{ .bytes = &mid }, .class = .entire_op, .file = World.striped(head.len) });
    try testing.expectEqual(@as(u32, 0), o2.count(.overlay));
    try testing.expectEqual(installs0, device_overlay.stats.overlay_installs.load());
    try testing.expectEqual(declines0 + 1, device_overlay.stats.overlay_ineligible_ram_custody.load());
    // The flush: ONE block carries both pieces, the rest zeros.
    w.wp.flushIno(io, 1);
    w.wp.quiesce(io);
    const b = w.f.map.seam().lookup(1, 0) orelse return error.BlockUnmapped;
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(b.vol_tag, b.offset, &got);
    try testing.expectEqualSlices(u8, &head, got[0..head.len]);
    try testing.expectEqualSlices(u8, &mid, got[8 * PAGE .. 12 * PAGE]);
    try testing.expect(std.mem.allEqual(u8, got[head.len .. 8 * PAGE], 0));
    try testing.expect(std.mem.allEqual(u8, got[12 * PAGE ..], 0));
}

// generic/551's stall: the overlay store took its depth-governor permit
// UNDER the block guard, and the in-flight permits belonged to uploads
// that re-take that guard to publish — four overlay writers parked at the
// governor holding the file's four block locks, ten uploads parked on
// those locks holding the whole depth, 53 writes in flight for 30 min
// (`551-stall-stats.json`: `write_pipeline_inflight_bytes` ≡ the target,
// `lock_ticked_reregisters` 41 k). The law the accumulation arm already
// kept ("the governor permit AFTER the guard") binds every arm.
test "a writer parked at the write-pipeline governor holds no block guard: the overlay arm meeting a full pipe drops (3) for its park and completes when the in-flight upload lands" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .{ .pinned = 1 }, true);
    defer w.deinit(testing.allocator);
    var held: HeldDma = .{};
    w.f.dev.hook = held.hook();
    // ino 2: an accumulation block completes (unaligned pieces: never the
    // overlay); its detached upload holds the whole depth (one block) in
    // its DMA, with the block guard DROPPED for the device phase.
    try w.f.map.create(2, .striped, 0);
    var first: [wt.BS / 2 + 100]u8 = undefined;
    fill(&first, 0x20);
    _ = try w.wp.write(io, .{ .ino = 2, .offset = 0, .payload = .{ .bytes = &first }, .class = .entire_op, .file = World.striped(0) });
    var rest: [wt.BS / 2 - 100]u8 = undefined;
    fill(&rest, 0x21);
    const o2 = try w.wp.write(io, .{ .ino = 2, .offset = first.len, .payload = .{ .bytes = &rest }, .class = .entire_op, .file = World.striped(first.len) });
    try testing.expectEqual(@as(u32, 1), o2.count(.write_through));
    while (held.hits.load(.acquire) == 0) std.Thread.yield() catch {};
    try testing.expectEqual(wt.BS, w.wp.governor.core.inflightBytes());
    // ino 3: an overlay-shaped fresh piece meets the full pipe.
    try w.f.map.create(3, .striped, 0);
    const waits0 = pipeline.stats.write_pipeline_admission_waits.load();
    const Writer = struct {
        w: *World,
        mid: [4 * PAGE]u8 align(4096) = undefined,
        result: anyerror!Outcome = error.NotRun,
        fn run(self: *@This()) void {
            self.result = self.w.wp.write(testing.io, .{ .ino = 3, .offset = 0, .payload = .{ .bytes = &self.mid }, .class = .entire_op, .file = World.striped(0) });
        }
    };
    var writer: Writer = .{ .w = w };
    fill(&writer.mid, 0x30);
    const th = try std.Thread.spawn(.{}, Writer.run, .{&writer});
    while (pipeline.stats.write_pipeline_admission_waits.load() == waits0) std.Thread.yield() catch {};
    // THE LAW: parked at the governor, the writer holds no block guard —
    // the upload that will return the depth could take it now. (The
    // verdict is judged after the join: a world torn down under a parked
    // writer is no verdict.)
    const guard_free = if (w.wp.locks.tryAcquire(io, 3, 0)) |g| blk: {
        g.release(io);
        break :blk true;
    } else false;
    // The DMA lands: the upload publishes and completes its permit; the
    // writer is admitted, re-takes the guard and stores through the overlay.
    held.release();
    th.join();
    try testing.expect(guard_free);
    const o3 = try writer.result;
    try testing.expectEqual(@as(u32, 1), o3.count(.overlay));
    w.wp.quiesce(io);
    w.wp.flushIno(io, 3);
    w.wp.quiesce(io);
    try testing.expectEqual(@as(u64, 0), w.wp.governor.core.inflightBytes());
    const b = w.f.map.seam().lookup(3, 0) orelse return error.BlockUnmapped;
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(b.vol_tag, b.offset, &got);
    try testing.expectEqualSlices(u8, &writer.mid, got[0 .. 4 * PAGE]);
}

/// A settler of one ino's custody on its own thread (the truncate's venue).
const Settler = struct {
    w: *World,
    ino: u64,
    done: std.atomic.Value(bool) = .init(false),

    fn run(self: *Settler) void {
        self.w.wp.settleCustody(testing.io, self.ino);
        self.done.store(true, .release);
    }
};

// fstests generic/074 (`fstest`, 3 writers with truncate loops) and 464
// (16 `xfs_io -ftc` loops over 200 files): ONE open(O_TRUNC) parked at
// `handler_entry` for 35–40 min, no lock held, every thread asleep,
// `lock_ticked_reregisters` 0. The pre-clip settle waited for EVERY ino's
// detached uploads through the write path's one task set — a single-
// awaiter group join: a second truncate settling at once overwrote the
// first's awaiter cell, and the last upload's exit woke only one of them.
// The settle of an ino owes nothing to another ino's upload.
test "the pre-clip custody settle of one ino waits for nothing of another: two settles at once return while a third ino's upload is held in its DMA" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .adaptive, false);
    defer w.deinit(testing.allocator);
    var held: HeldDma = .{ .first_only = true };
    w.f.dev.hook = held.hook();
    // ino 2: a block completes; its detached upload is held in its DMA.
    try w.f.map.create(2, .striped, 0);
    var half: [wt.BS / 2]u8 align(4096) = undefined;
    fill(&half, 0x20);
    _ = try w.wp.write(io, .{ .ino = 2, .offset = 0, .payload = .{ .bytes = &half }, .class = .entire_op, .file = World.striped(0) });
    const o2 = try w.wp.write(io, .{ .ino = 2, .offset = wt.BS / 2, .payload = .{ .bytes = &half }, .class = .entire_op, .file = World.striped(wt.BS / 2) });
    try testing.expectEqual(@as(u32, 1), o2.count(.write_through));
    while (held.hits.load(.acquire) == 0) std.Thread.yield() catch {};
    // Two truncates of OTHER inos settle at once (each with one parked
    // partial of its own, so the settle has real work).
    try parkHalf(w, io, 1, 0, 0x11);
    try parkHalf(w, io, 3, 0, 0x33);
    var s1: Settler = .{ .w = w, .ino = 1 };
    var s3: Settler = .{ .w = w, .ino = 3 };
    const t1 = try std.Thread.spawn(.{}, Settler.run, .{&s1});
    const t3 = try std.Thread.spawn(.{}, Settler.run, .{&s3});
    // Both return while ino 2's DMA is still held. The bound is one
    // liveness tick: a settle that owes nothing parks nowhere, and the old
    // form never returned before the release (the verdict is judged after
    // the release and the joins — a world torn down under a parked settler
    // is no verdict).
    const deadline = clock.monotonicNs() + sync_tick.tick_ns;
    while ((!s1.done.load(.acquire) or !s3.done.load(.acquire)) and clock.monotonicNs() < deadline) std.Thread.yield() catch {};
    const both_returned_while_held = s1.done.load(.acquire) and s3.done.load(.acquire) and held.gate.load(.acquire) == 0;
    held.release();
    t1.join();
    t3.join();
    w.wp.quiesce(io);
    try testing.expect(both_returned_while_held);
    // The settles did their own work: inos 1 and 3 are flushed and mapped;
    // ino 2's upload landed once released.
    try testing.expect(!w.wp.table.has(1, 0) and !w.wp.table.has(3, 0));
    try testing.expect(w.f.map.seam().lookup(1, 0) != null and w.f.map.seam().lookup(3, 0) != null);
    try testing.expect(w.f.map.seam().lookup(2, 0) != null);
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
