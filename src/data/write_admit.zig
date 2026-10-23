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
const lease_epoch = @import("lease_epoch.zig");
const sole_owner_patch = @import("sole_owner_patch.zig");
const accumulate = @import("accumulate.zig");
const extent = @import("extent_overlay.zig");
const writeback = @import("writeback.zig");
const transform = @import("transform.zig");
const fsync_mod = @import("fsync.zig");
const write_side_view = @import("write_side_view.zig");

pub const Binding = seams.Binding;
pub const PAGE: u32 = seams.PAGE;

/// The extension law's zeros (`WritePath.zeroExtension`): one block at the
/// format's largest (`transform.MAX_BLOCK_SIZE`), never written —
/// zero-initialized static storage, so no image bytes and no RSS beyond the
/// shared zero page.
var gap_zeros: [transform.MAX_BLOCK_SIZE]u8 align(PAGE) = @splat(0);

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
                .hold_slot = knobs.boolKnob("SQUEEZEFS_ZC_HOLD_OVERLAY", env),
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
        if (!self.overlay.hold_slot) log.info("write path: A/B lever SQUEEZEFS_ZC_HOLD_OVERLAY=0 (overlay-class writes extracted at delivery)", .{});
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
    /// `write_gap_zero_bytes`: the zeros the extension law admitted
    /// (`WritePath.zeroExtension`, decision 0003 §AL) — bytes the
    /// filesystem itself wrote, so the admission ledger ≡ WRITE payload
    /// bytes + these.
    pub var write_gap_zero_bytes: counters.ShardedCounter = .{};
    /// `fallocate_blocks_reserved`: holes bound to fresh UNWRITTEN blocks
    /// (`reserveRange`); `fallocate_blocks_zeroed`: written blocks flipped
    /// to their unwritten twin by ZERO_RANGE — no device byte for either.
    pub var fallocate_blocks_reserved: counters.ShardedCounter = .{};
    pub var fallocate_blocks_zeroed: counters.ShardedCounter = .{};
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
    inline for (.{ "admit_write_overlay_bytes", "admit_write_inplace_bytes", "admit_write_accumulate_bytes", "admit_write_inline_bytes", "write_enospc_refusals", "write_pieces", "writes_admitted", "staging_sibling_probes", "restage_churn_removes", "restage_churn_bytes", "parked_gate_waits", "parked_gate_self_flushes", "parked_gate_cold_flushes", "parked_gate_wall_refusals", "write_gap_zero_bytes", "fallocate_blocks_reserved", "fallocate_blocks_zeroed" }) |name| {
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
    /// The ino's size epoch as the last piece landed (`MapSeam.sizeEpoch`):
    /// the postlude asks the plane whether a size-setting commit landed
    /// since (`DataPlane.sizeSetSince`) and parks no claim when one did —
    /// the truncate clipped the bytes `size_claim` would name. 0 for a
    /// write that landed nothing.
    size_epoch: u64,
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
    /// The stored-image scratches (`scratch_pool_cap` × `scratch_len`), one
    /// per upload in flight through its DMA; null on an all-passthrough set.
    scratch: ?*transform.ScratchPool,
    /// Some active volume is passthrough (the overlay has a destination).
    any_passthrough: bool,
    /// The plaintext whole-block image of a binding (the plane decodes a
    /// transformed frame; null = raw device bytes, an all-passthrough set).
    image: ?seams.ImageSeam,
    /// The volume's FAILED/fenced probe every fence arm classifies on
    /// (`seams.classifyFence`): the overlay's, the pipeline's, the epochs'.
    fence: ?seams.FenceProbe,
    /// The write cache's resident footprint — its claim on the store's
    /// free bytes ahead of every fresh custody (`enospc.storeCoversRing`);
    /// the plane installs it with its staging ring, null = no write cache.
    ring_claim: ?RingClaim = null,

    pub const RingClaim = struct {
        ctx: *anyopaque,
        bytes: *const fn (*anyopaque) u64,

        pub fn load(self: RingClaim) u64 {
            return self.bytes(self.ctx);
        }
    };

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
        /// `transform.scratchPoolCap(cpus)`: the scratches the pool holds
        /// when `scratch_len != 0`.
        scratch_pool_cap: u32 = 0,
        /// The plaintext image seam (required when any volume is transformed).
        image: ?seams.ImageSeam = null,
        /// The volume's FAILED latch (the plane's `fencePoll`): the
        /// overlay's settle ladder ends on it, and every fence drop of the
        /// write path is classified on it (`seams.classifyFence`).
        fence: ?seams.FenceProbe = null,
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
            .fence = d.fence,
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
        if (d.scratch_len != 0) {
            const pool = try gpa.create(transform.ScratchPool);
            errdefer gpa.destroy(pool);
            pool.* = try transform.ScratchPool.init(gpa, d.scratch_pool_cap, d.scratch_len);
            self.scratch = pool;
        }
        errdefer if (self.scratch) |p| {
            p.deinit(gpa);
            gpa.destroy(p);
        };
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
            .fence = d.fence,
            .touched = &touchedStamp,
            .touched_ctx = self,
        }, device_overlay.registryCapacity(cfg.parked_cap, pipeline.capBytes(d.budget_bytes), cfg.block_size));
        self.view = .{ .table = &self.table, .overlay = &self.overlay, .epochs = &self.epochs, .locks = &self.locks, .device = d.device };
        return self;
    }

    pub fn deinit(self: *WritePath, io: Io) void {
        self.tasks.deinit(io);
        self.overlay.deinit(io);
        if (self.scratch) |p| {
            p.deinit(self.gpa);
            self.gpa.destroy(p);
        }
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
        return .{ .map = self.map, .free = self.free, .purge = self.purge, .fence = self.fence };
    }

    fn pressureHook(ctx: ?*anyopaque) bool {
        const self: *WritePath = @ptrCast(@alignCast(ctx.?));
        return self.epochs.closeAllForSpace(self.io, self.closeSeams());
    }

    pub fn uploadDeps(self: *WritePath) accumulate.UploadDeps {
        return .{ .device = self.device, .map = self.map, .free = self.free, .purge = self.purge, .vols = self.vols, .picker = self.picker.picker(), .valve = self.valve, .epochs = &self.epochs, .stripes = self.stripes, .locks = &self.locks, .transform = self.transform, .scratch = self.scratch, .touched = &touchedStamp, .touched_ctx = self, .image = self.image, .custody = .{ .ctx = self, .ram_custody = &epochCustody, .overlay_open = &overlayOpen }, .fence = self.fence };
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

    /// The direct WRITE lease's per-block oracle (decision 0003 §AY): the
    /// W1 state half under a TRY-locked block guard — custody, the
    /// authoritative map, passthrough, whole and undamaged, the durable
    /// sole-owner verdict and the fence pair — then the word re-stabilized
    /// under a new generation (nothing was written). `.eligible` = a client
    /// may patch this block in place under a lease; `.busy` = the block
    /// guard is held (the caller refuses `moved`, the client retries);
    /// `.ineligible` = one of W1's own declines (shared, decorated,
    /// foreign, custody): the ring serves the file's writes.
    pub const LeaseProbe = enum { eligible, busy, ineligible };

    pub fn probeDirectWriteLease(self: *WritePath, io: Io, ino: u64, block: u32) LeaseProbe {
        const guard = self.locks.tryAcquire(io, ino, block) orelse return .busy;
        defer guard.release(io);
        const verdict = sole_owner_patch.beginInPlace(self.patchDeps(), ino, block, 0) catch return .ineligible;
        switch (verdict) {
            .begun => |b| {
                sole_owner_patch.abandonInPlace(self.patchDeps(), b);
                return .eligible;
            },
            .declined => return .ineligible,
        }
    }

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

    /// The write cache's claim is UNCOVERED: the set's reachable free bytes
    /// no longer cover the ring's resident footprint plus `extra` B of
    /// growth (`enospc.storeCoversRing`; the module doc's terminal law, the
    /// sync point's half). One atomic load while the ring is empty; false
    /// with no write cache.
    pub fn ringUncovered(self: *const WritePath, extra: u64) bool {
        const claim = self.ring_claim orelse return false;
        const ring = claim.load();
        if (ring == 0 and extra == 0) return false;
        return !enospc.storeCoversRing(self.vols.reachableFreeBytes(self.cfg.block_size), ring, extra);
    }

    /// RAM-only: a block is HELD iff custody exists (parked, overlay, epoch),
    /// the write can land IN PLACE (passthrough ∧ undecorated whole-block
    /// ∧ active volume ∧ `refcount == 1 ∧ !SHARED`) or the file holds a
    /// RESERVED block there it owns alone (a fallocate reservation lands in
    /// place, charter §6.6 — on any volume class: the whole image is
    /// written); a hole below EOF holds nothing; a cache miss answers
    /// "held" (the refusal must never fire on a block whose mapping it
    /// could not see).
    pub fn writeWouldCreateFreshCustody(self: *WritePath, ino: u64, file: ?seams.FileView, first: u32, last: u32) bool {
        const f = file orelse return false;
        if (!f.map_resident) return false;
        var b = first;
        while (b <= last) : (b += 1) {
            if (self.table.has(ino, b) or self.overlay.hasCustody(ino, b) or self.epochs.bindsBlock(ino, b)) continue;
            if (self.map.reserved(ino, b)) |r| {
                const v = self.vols.byTag(r.vol_tag) orelse return true;
                if (!v.isActive()) return true;
                const idx = v.alloc.idxOf(r.offset);
                if (v.alloc.refcount(idx) != 1 or v.alloc.isShared(idx)) return true;
                continue;
            }
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
        if (len == 0) return .{ .written = 0, .size_claim = 0, .size_epoch = 0, .pieces = 0 };
        const end = std.math.add(u64, req.offset, len) catch return error.FileTooBig;
        if (end / bs >= std.math.maxInt(u32)) return error.FileTooBig;
        const file = req.file orelse seams.FileView{ .kind = .striped, .size = 0, .map_resident = false };
        // The gap this write opens past EOF lands first (the extension law):
        // the write then continues it, stream-adjacent, in the same custody.
        if (req.offset > file.size) try self.zeroExtension(io, req.ino, file, req.offset);
        // The shared-mode handoff: a Shared classification is a cache-
        // resident striped write with `end ≤ size` — anything else here is
        // the handler's classifier disagreeing with the data plane.
        if (req.class == .shared and (file.kind != .striped or end > file.size)) {
            stats.write_shared_class_violation.fire("a Shared-classified write extends the file or is not striped");
            log.err("write ino {d} [{d}, {d}): classified Shared against a {t} file of size {d}", .{ req.ino, req.offset, end, file.kind, file.size });
        }
        const first: u32 = @intCast(req.offset / bs);
        const last: u32 = @intCast((end - 1) / bs);
        // §8: the ENOSPC terminal law, decided for the WHOLE write: the
        // set's fresh supply exhausted, or the write cache's claim on the
        // store uncovered (`ringUncovered`). The custody probe (a map lookup
        // per block) is asked only once one of them holds: otherwise the law
        // admits whatever it would answer.
        var cands: [router.MAX_VOLUMES]*block_alloc.BlockAllocator = undefined;
        const exhausted = enospc.freshSupplyExhausted(self.vols.candidates(&cands));
        const uncovered = !exhausted and self.ringUncovered(0);
        if (enospc.admitWrite(exhausted, uncovered, (exhausted or uncovered) and self.writeWouldCreateFreshCustody(req.ino, req.file, first, last)) == .refuse_enospc) {
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
        var out: Outcome = .{ .written = 0, .size_claim = end, .size_epoch = 0, .pieces = @intCast(pieces.len) };
        for (pieces) |pc| {
            stats.write_pieces.inc();
            const arm = try self.admitPiece(io, req, file, pc, &bytes, try_patch and single, overlay_shape, stream_adjacent);
            out.arms[@backingInt(arm)] += 1;
            out.written += pc.len;
        }
        // Read AFTER the last piece landed: a size set before this point
        // saw (or settled) the pieces it clipped; one after it is the
        // postlude's to detect.
        out.size_epoch = self.map.sizeEpoch(req.ino);
        stats.writes_admitted.inc();
        return out;
    }

    /// THE EXTENSION LAW (decision 0003 §AL; charter §6.2 "uncovered bytes
    /// never escape"): growing `file` from its EOF to `upto` — a write past
    /// EOF, a truncate-up — writes `[EOF, min(upto, the EOF block's end))`
    /// of a MAPPED EOF block as the write path's own zeros, admitted like
    /// any byte, so every arm lands them (in place: their pages; CoW: with
    /// the block) and every reader serves them from custody until then. A
    /// mapped block's bytes past EOF are not the file's: a sub-block landing
    /// (a promoted inline, staged or packed image — `plane.ownBlockStored`
    /// DMAs its grain) leaves the tail the block's previous owner wrote (the
    /// block-device free elides its discard: reclaim.zig), a shrink leaves
    /// the file's own old bytes, and every seed copies them forward. A
    /// hole's EOF block reads zeros and a block-aligned EOF opens no gap in
    /// a mapped block: nothing is written. Cost: zeros ≤ the growth inside
    /// the EOF block, landed with the write that continues them — §AK's
    /// `pages × P` in place, never a second landing.
    pub fn zeroExtension(self: *WritePath, io: Io, ino: u64, file: seams.FileView, upto: u64) Error!void {
        const bs = self.cfg.block_size;
        const eof = file.size;
        if (upto <= eof or eof % bs == 0) return;
        if (eof / bs >= std.math.maxInt(u32)) return error.FileTooBig;
        // Unguarded: the grower holds the ino's exclusive op guard, so only
        // a landing changes the EOF block's binding meanwhile — one binding
        // for another, or a fresh custody's zero-completed block.
        if (self.epochs.baseBinding(self.map, ino, @intCast(eof / bs)) == null) return;
        const n: usize = @intCast(@min(upto, std.mem.alignForward(u64, eof + 1, bs)) - eof);
        assert(n < gap_zeros.len);
        _ = try self.write(io, .{ .ino = ino, .offset = eof, .payload = .{ .bytes = gap_zeros[0..n] }, .class = .entire_op, .file = file });
        stats.write_gap_zero_bytes.add(n);
    }

    /// Zeros over `[offset, offset + len)` of a striped file, admitted like
    /// any byte (a PUNCH_HOLE / ZERO_RANGE edge inside a written block,
    /// charter §6.6): the extension law's static zero block, one write per
    /// block-bounded piece, so every arm lands them (W1 in place for an
    /// aligned sub-cap piece, the overlay, the accumulation arms).
    pub fn writeZeros(self: *WritePath, io: Io, ino: u64, offset: u64, len: u64, file: seams.FileView) Error!void {
        const bs = self.cfg.block_size;
        var at = offset;
        const end = offset + len;
        while (at < end) {
            const piece: usize = @intCast(@min(end - at, std.mem.alignForward(u64, at + 1, bs) - at));
            assert(piece <= gap_zeros.len);
            _ = try self.write(io, .{ .ino = ino, .offset = at, .payload = .{ .bytes = gap_zeros[0..piece] }, .class = .entire_op, .file = file });
            at += piece;
        }
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
                const stored = switch (try self.overlay.claim(io, .{ .ino = req.ino, .block = pc.block, .rel = pc.rel, .vehicle = vehicle, .file = file, .overwrite = overwrite, .permit = handed })) {
                    // The claim excludes an overlapping DMA, not (3): a
                    // sibling write into this block lands beside this one.
                    .claimed => |c| landed: {
                        held = false;
                        g.release(io);
                        break :landed try self.overlay.land(io, c);
                    },
                    else => |o| o,
                };
                switch (stored) {
                    .claimed => unreachable, // landed above
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
            //    A stream-adjacent piece CONTINUES an overlay already parked
            //    over a mapped base (a held-open appender's first write
            //    parked here): escalating it would seed the whole block to
            //    land a few touched pages (decision 0003 §AK).
            //    A first piece past the extent cap takes G's full buffer
            //    (decision 0003 §AP), never the gate.
            const small = extent.isSmall(pc.len, bs);
            const existing = self.table.get(req.ino, pc.block);
            const base_passthrough = if (live_binding) |b| (if (self.vols.byTag(b.vol_tag)) |v| v.passthrough else false) else true;
            const continues_overlay = if (existing) |x| x.repr == .extent and x.repr.extent.base_deferred else false;
            if (small and (!stream_adjacent or continues_overlay) and (existing == null or existing.?.repr == .extent) and (existing != null or base_passthrough)) arm_f: {
                const e = existing orelse self.table.checkoutExtent(req.ino, pc.block, has_existing, live_binding) orelse break :arm_f;
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
        // The read leases (decision 0003 §AX.3): every arm but the in-place
        // patch leaves bytes the device does not hold (an overlay record, a
        // RAM extent, a staged file) — a direct client's cached layout of
        // the inode is stale from here. The patch landed on the device: the
        // leases stand (a direct client's own writes bump nothing).
        if (arm != .patch) lease_epoch.bump(req.ino);
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
                self.table.fillComplementFrom(e, img, old) catch |e2| switch (e2) {
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
                    .stay_parked => {
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
        // The write-through trigger owes nothing: a superseded snapshot
        // publishes and the newer writes wait for their own drain.
        const d = accumulate.uploadParkedBlock(&self.table, self.uploadDeps(), io, ino, block, 0, prof);
        if (permit) |p| {
            const ord: u32 = if (self.map.lookup(ino, block)) |b| (if (self.vols.byTag(b.vol_tag)) |v| v.ordinal else 0) else 0;
            self.governor.complete(io, p, ord, clock.monotonicNs() -| p.admitted_ns);
        }
        prof.finish();
        pipeline.countDisposition(d, ino, block, self.fence);
    }

    /// Wait for every detached upload/drain (tests, teardown).
    pub fn quiesce(self: *WritePath, io: Io) void {
        self.tasks.wait(io) catch {};
        self.overlay.tasks.wait(io) catch {};
    }

    /// The D3 fold targets (`defrag --fold`, `ops_jobs_lifecycle.md` §3.1):
    /// the distinct inos holding a parked W2 extent entry, ascending; owned
    /// by `gpa`.
    pub fn foldTargets(self: *WritePath, gpa: Allocator) Allocator.Error![]u64 {
        const keys = try self.table.extentKeys(gpa);
        defer gpa.free(keys);
        var inos: std.ArrayList(u64) = .empty;
        errdefer inos.deinit(gpa);
        for (keys) |k| {
            var seen = false;
            for (inos.items) |i| if (i == k.ino) {
                seen = true;
                break;
            };
            if (!seen) try inos.append(gpa, k.ino);
        }
        std.mem.sort(u64, inos.items, {}, std.sort.asc(u64));
        return inos.toOwnedSlice(gpa);
    }

    /// The D3 kick: every parked extent entry of `ino` through the
    /// EXISTING W2 fold (`accumulate.foldExtentBlock`: in place at the
    /// page grain for a sole-owner passthrough block, else the seeded
    /// escalation that parks a content-valid full buffer) — never a
    /// reimplementation. Returns the entries that left the extent class
    /// (retired, or escalated to a full buffer); an entry the pool cap
    /// kept parked counts 0 and the next pass sees it again.
    pub fn foldParked(self: *WritePath, io: Io, ino: u64) u32 {
        var blocks: [64]u32 = undefined;
        var folded: u32 = 0;
        for (self.table.blocksOf(ino, &blocks)) |b| {
            const was_extent = if (self.table.get(ino, b)) |e| e.repr == .extent else false;
            if (!was_extent) continue;
            accumulate.foldExtentBlock(&self.table, self.uploadDeps(), io, ino, b);
            const still_extent = if (self.table.get(ino, b)) |e| e.repr == .extent else false;
            if (!still_extent) folded += 1;
        }
        return folded;
    }

    /// THE ORDERED LAW's floor (decision 0003 §AL.5): the lowest byte of
    /// `ino` its RAM write custody covers (`maxInt` when it holds none).
    /// Every byte below it the file wrote is on a device (behind the data
    /// barrier) or in the staging ring, so a size claim up to it may be
    /// journaled. A parked block answers its lowest covered byte (read
    /// under its (3): a continuous appender's tail block never pins the
    /// size at its block start); an open overlay record answers its block's
    /// start; a rewrite epoch — open or saving — answers its lowest
    /// shadowed block's start: a shadowed block is named by no durable map
    /// until its save lands (a fresh block rides the epoch too, so an
    /// extending stream's claim follows its saves: the whole-epoch 0 of
    /// the overwrite-only epoch would have held a streamer's size at its
    /// first save for the stream's whole life).
    pub fn custodyFloor(self: *WritePath, io: Io, ino: u64) u64 {
        const bs = self.cfg.block_size;
        var floor: u64 = std.math.maxInt(u64);
        if (self.epochs.lowestBlockOf(ino)) |b| floor = @as(u64, b) * bs;
        if (self.overlay.registry.lowestBlockOf(ino)) |b| floor = @min(floor, @as(u64, b) * bs);
        if (self.table.lowestBlockOf(ino)) |b| {
            const g = self.locks.acquire(io, ino, b, .writeback_flush);
            defer g.release(io);
            // Retired since the scan: its block start (the next drain
            // reads the advance).
            const rel: u64 = if (self.table.get(ino, b)) |e| lowestCovered(e) else 0;
            floor = @min(floor, @as(u64, b) * bs + rel);
        }
        return floor;
    }

    /// A reservation's mint: the placed grace-bounded mint, with the
    /// allocation plane's RETRYABLE refusal (a grant window being refilled
    /// by a reachable holder) retried a slice at a time up to the park wall
    /// — the upload parks and comes back on its next pass, a synchronous
    /// fallocate has no next pass; past the wall the refusal stands (EIO:
    /// fallocate(2) owns no EAGAIN).
    fn reserveMint(self: *WritePath, io: Io, ino: u64, block: u32) enospc.PlacedError!enospc.PlacedMint {
        const started = clock.monotonicNs();
        const wall_ns = enospc.pressureParkWallMs(0) * std.time.ns_per_ms;
        while (true) {
            return enospc.allocatePlacedMint(self.picker.picker(), .{ .ino = ino, .block = block }, io, self.valve, .{ .ctx = self, .run = &pressureHook }, false, .visible_supply) catch |e| switch (e) {
                error.PlaneRefused => {
                    if (clock.monotonicNs() - started >= wall_ns) return e;
                    io.sleep(.fromMilliseconds(@intCast(enospc.pressureParkSliceMs(0))), .awake) catch {};
                    continue;
                },
                else => e,
            };
        }
    }

    pub const Reserved = struct {
        /// Holes bound to fresh UNWRITTEN blocks.
        reserved: u32 = 0,
        /// Written blocks flipped to their unwritten twin (ZERO_RANGE).
        zeroed: u32 = 0,
    };

    /// THE RESERVATION (charter §6.6; decision 0003 §BH.2): every hole of
    /// `[first, last]` bound to a fresh UNWRITTEN block — minted, published
    /// in the allocator, never written — in merges of at most
    /// `MergeOutcome.max_displaced` inserts each (one journal entry); with
    /// `zero_existing` (ZERO_RANGE) every WRITTEN block of the range flips
    /// to its unwritten twin in the same merges — the same reference, no
    /// device I/O — and leaves every read tier. A block already reserved is
    /// left alone. The mint waits for visible supply as the upload's does;
    /// a full store refuses ENOSPC with the chunk's mints returned and the
    /// earlier chunks standing (fallocate is idempotent). The caller holds
    /// the inode's exclusive op guard with its write custody settled, so
    /// no landing races the flip; the 4a guards are not held (the merge
    /// takes the (3.5) stripe and the (3.7) layout lock).
    pub fn reserveRange(self: *WritePath, io: Io, ino: u64, first: u32, last: u32, zero_existing: bool) Error!Reserved {
        var out: Reserved = .{};
        const cap = seams.MergeOutcome.max_displaced;
        var b: u64 = first;
        while (b <= last) {
            var inserts: [cap]seams.MapInsert = undefined;
            var mints: [cap]block_alloc.MintedBlockGuard = undefined;
            var guards: [cap]block_alloc.InflightAllocGuard = undefined;
            var flips: [cap]Binding = undefined;
            var n: usize = 0;
            var n_mints: usize = 0;
            var n_flips: usize = 0;
            var committed = false;
            defer {
                for (guards[0..n_mints]) |*g| g.deinit();
                if (!committed) for (mints[0..n_mints]) |*m| m.abandon();
            }
            while (b <= last and n < cap) : (b += 1) {
                const blk: u32 = @intCast(b);
                if (self.map.reserved(ino, blk) != null) continue;
                if (self.map.lookup(ino, blk)) |m| {
                    if (!zero_existing) continue;
                    inserts[n] = .{ .block = blk, .binding = m.withUnwritten(true) };
                    flips[n_flips] = m;
                    n += 1;
                    n_flips += 1;
                    continue;
                }
                const pm = self.reserveMint(io, ino, blk) catch |e| return switch (e) {
                    error.StorageFull, error.NoHealthyBackends => error.NoSpaceLeft,
                    error.ReadOnly => error.ReadOnlyFileSystem,
                    error.PlaneRefused, error.NotFree => error.InputOutput,
                };
                mints[n_mints] = block_alloc.MintedBlockGuard.init(pm.alloc, pm.minted);
                guards[n_mints] = block_alloc.InflightAllocGuard.register(pm.alloc, pm.minted.idx);
                n_mints += 1;
                // Allocated, never DMA'd: the word is stable at once (a
                // landing's fence pair retires and re-publishes it).
                pm.alloc.publishBlock(pm.minted.idx);
                inserts[n] = .{ .block = blk, .binding = Binding.unwrittenBlock(pm.alloc.volTag(), pm.minted.offset, pm.minted.stamp) };
                n += 1;
            }
            if (n == 0) continue;
            var displaced: seams.MergeOutcome = .{};
            {
                const sg = self.stripes.acquire(io, ino);
                defer sg.release(io);
                displaced = while (true) {
                    break self.map.merge(.{ .ino = ino, .inserts = inserts[0..n], .min_size = 0, .proofs = &.{}, .fencing = self.map.currentFencing(ino) }) catch |e| switch (e) {
                        error.FencingTokenExpired => continue,
                        error.WriterGuardFenced => {
                            // W5: free nothing.
                            for (mints[0..n_mints]) |*m| m.disarm();
                            return error.WriterGuardFenced;
                        },
                        error.NotFound => return error.InputOutput,
                        error.Transient => return error.InputOutput,
                        error.OutOfMemory => return error.OutOfMemory,
                    };
                };
            }
            committed = true;
            for (mints[0..n_mints]) |*m| m.disarm();
            // A flipped block's bytes are nobody's now: out of every tier
            // before the next landing can make the key reachable again.
            for (flips[0..n_flips]) |m| self.purge.purge(m);
            // Same references throughout: nothing displaced by construction.
            for (displaced.slice()) |d| _ = self.free.free(io, d);
            displaced.dropHolds();
            out.reserved += @intCast(n_mints);
            out.zeroed += @intCast(n_flips);
        }
        stats.fallocate_blocks_reserved.add(out.reserved);
        stats.fallocate_blocks_zeroed.add(out.zeroed);
        return out;
    }

    fn lowestCovered(e: *const accumulate.Entry) u64 {
        switch (e.repr) {
            .full => {
                var buf: [cov.CoverageUnion.max_runs]cov.Run = undefined;
                const runs = e.coverage.runsSorted(&buf);
                return if (runs.len == 0) 0 else runs[0].start;
            },
            .extent => |*x| {
                var buf: [extent.MAX_PAGES]extent.Run = undefined;
                const runs = x.pageRuns(&buf);
                return if (runs.len == 0) 0 else runs[0].start;
            },
        }
    }

    /// Whether `ino` holds any custody `flushIno` lands — a parked block (a
    /// W2 extent among them, an upload in flight), an overlay record, a
    /// rewrite epoch open or saving: the no-open FLUSH's try-only probe
    /// (decision 0003 §AF). Spin-locked O(1)-or-scan sections only — never
    /// a device wait, a park, or a lock held across I/O.
    pub fn holdsCustody(self: *WritePath, ino: u64) bool {
        return self.table.holdsIno(ino) or self.overlay.registry.holdsIno(ino) or self.epochs.holds(ino);
    }

    /// What the write-back timer's drain of one ino did (decision 0003
    /// §AJ): `landed` — custody born by the cutoff was held and landed;
    /// `kept_born_ns` — the birth of the oldest custody still held.
    pub const AgedDrain = struct { landed: bool, kept_born_ns: ?u64 };

    /// The write-back timer's drain (decision 0003 §AJ): the overlay
    /// records drain and the epoch closes, each whole (a record's bytes
    /// already sit on the device; the epoch is the ino's one save); the
    /// parked blocks born at or before `cutoff_ns` flush through
    /// `flushParkedBlock` (the RELEASE drain's own landing), the younger
    /// ones stay — a block filled within the age writes itself through,
    /// so a streamer's in-progress block never lands partial. `landed`
    /// counts a landing that happened (published, or joined to an upload
    /// that retired the entry). What stays held afterwards — a young block,
    /// a flush that failed or was superseded (aged: retried by the next
    /// pass), custody a write made meanwhile (its own note owes it) — comes
    /// back as the oldest birth.
    pub fn drainAged(self: *WritePath, io: Io, ino: u64, cutoff_ns: u64) AgedDrain {
        var landed = self.overlay.registry.holdsIno(ino) or self.epochs.holds(ino);
        _ = self.overlay.drainForIno(io, &self.locks, ino, std.math.maxInt(u64), false, self.closeSeams());
        var births: [64]accumulate.Table.BlockBirth = undefined;
        for (self.table.birthsOf(ino, &births)) |b| {
            if (b.born_ns > cutoff_ns) continue;
            switch (accumulate.flushParkedBlockBy(&self.table, self.uploadDeps(), io, ino, b.block, cutoff_ns)) {
                .published, .joined => landed = true,
                .stay_parked, .fence_drop, .discarded => {},
            }
        }
        _ = self.epochs.close(io, ino, self.closeSeams());
        var kept: ?u64 = null;
        for (self.table.birthsOf(ino, &births)) |b| kept = @min(kept orelse b.born_ns, b.born_ns);
        if (self.overlay.registry.holdsIno(ino) or self.epochs.holds(ino)) kept = @min(kept orelse cutoff_ns, cutoff_ns);
        return .{ .landed = landed, .kept_born_ns = kept };
    }

    /// RELEASE / unmount: every parked block of `ino` flushes durably, the
    /// overlay drains (write-back class), the epoch closes.
    pub fn flushIno(self: *WritePath, io: Io, ino: u64) void {
        _ = self.overlay.drainForIno(io, &self.locks, ino, std.math.maxInt(u64), false, self.closeSeams());
        _ = accumulate.flushBornBy(&self.table, self.uploadDeps(), io, ino, clock.monotonicNs());
        _ = self.epochs.close(io, ino, self.closeSeams());
    }

    /// Every ino holding write-side custody right now — a parked block, an
    /// open overlay record, a rewrite epoch open or saving — ascending and
    /// distinct in `out` (the syncfs census's population, `fsync.syncfs`).
    pub fn custodyInos(self: *WritePath, gpa: Allocator, out: *std.ArrayList(u64)) Allocator.Error![]u64 {
        var keys: std.ArrayList(accumulate.Key) = .empty;
        defer keys.deinit(gpa);
        try self.table.censusKeys(gpa, &keys);
        try out.ensureUnusedCapacity(gpa, keys.items.len);
        for (keys.items) |k| out.appendAssumeCapacity(k.ino);
        try self.overlay.registry.censusInos(gpa, out);
        try self.epochs.censusInos(gpa, out);
        std.mem.sort(u64, out.items, {}, std.sort.asc(u64));
        return fsync_mod.dedupeSorted(out.items);
    }

    /// The unmount's landing (charter §8 `umount`: an orderly unmount
    /// leaves no acked write in RAM): every ino the census finds holding
    /// custody through the RELEASE drain (`flushIno`) — the size
    /// claimants' and the size-neutral writers' alike (an overwrite inside
    /// a file under the write-back timer parks custody and no claim:
    /// fstests generic/029 lost such a tail at an unmount within T_wb).
    /// The staged residents are the dismount pass's (`DataPlane.dismount`).
    pub fn flushAll(self: *WritePath, io: Io) void {
        var inos: std.ArrayList(u64) = .empty;
        defer inos.deinit(self.gpa);
        const holders = self.custodyInos(self.gpa, &inos) catch {
            log.err("the unmount's landing pass could not list the custody holders (out of memory): their bytes stay in RAM", .{});
            return;
        };
        for (holders) |ino| self.flushIno(io, ino);
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
    try testing.expect(c.fsync.touched_namespaces and c.fsync.parallel_legs);
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
        return initSized(gpa, depth, overlay_on, 8, 32);
    }

    /// `init` with the parked cap and the blocks per volume a row parking
    /// many blocks needs.
    fn initSized(gpa: Allocator, depth: pipeline.DepthOverride, overlay_on: bool, parked_cap: u64, blocks_per_vol: u64) !*World {
        const w = try gpa.create(World);
        errdefer gpa.destroy(w);
        w.f = try wt.Fixture.init(gpa, blocks_per_vol);
        w.stripes = .{};
        const cfg: Config = .{
            .block_size = wt.BS,
            .patch_max = wt.BS / 8,
            .overlay = .{ .block_size = wt.BS, .patch_max = wt.BS / 8, .enabled = overlay_on },
            .rewrite_shadow = true,
            .parked_cap = parked_cap,
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

test "the fallocate reservation (charter §6.6, decision 0003 §BH): `reserveRange` binds every hole of the range to a fresh UNWRITTEN block with no device byte written and leaves written blocks alone; the map's `lookup` sees a hole there and `reserved` the binding; a write inside the reservation lands IN PLACE (the whole block into the reserved one, the publish flips the flag: the same reference, the allocator's census unchanged, no mint, no free); ZERO_RANGE flips a written block to its unwritten twin with no I/O and purges it; a reservation on a clone-shared block is left to the fresh-mint path, which displaces it" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, false);
    defer w.deinit(testing.allocator);
    // Block 0 written, blocks 1–3 holes; the file reaches 4 blocks.
    _ = try w.f.publishBlock(30, 0, 0xAA);
    w.f.map.files.getPtr(30).?.size = 4 * wt.BS;
    const used_a0 = w.f.alloc_a.usedBlocks();
    const used_b0 = w.f.alloc_b.usedBlocks();
    const writes0 = w.f.dev.writes.items.len;
    const r = try w.wp.reserveRange(io, 30, 0, 3, false);
    try testing.expectEqual(@as(u32, 3), r.reserved);
    try testing.expectEqual(@as(u32, 0), r.zeroed);
    try testing.expectEqual(writes0, w.f.dev.writes.items.len); // no device byte
    try testing.expectEqual(used_a0 + used_b0 + 3, w.f.alloc_a.usedBlocks() + w.f.alloc_b.usedBlocks()); // charged
    try testing.expectEqual(@as(u64, 3), w.f.map.refs_added); // one +ref per reservation, none for block 0
    const seam = w.f.map.seam();
    for (1..4) |b| {
        const blk: u32 = @intCast(b);
        try testing.expect(seam.lookup(30, blk) == null); // a hole to every arm
        const res = seam.reserved(30, blk).?;
        try testing.expect(res.unwritten and res.whole_block);
    }
    try testing.expect(seam.reserved(30, 0) == null);
    try testing.expect(seam.lookup(30, 0) != null);
    // Idempotent: a second reservation of the same range binds nothing.
    const again = try w.wp.reserveRange(io, 30, 0, 3, false);
    try testing.expectEqual(@as(u32, 0), again.reserved);
    try testing.expectEqual(@as(u64, 3), w.f.map.refs_added);
    // The write inside the reservation: a half block parks (a hole's
    // zero seed), its sibling completes the block — the landing takes the
    // reserved block over: no mint, the same reference, the flag flipped.
    const reserved_2 = seam.reserved(30, 2).?;
    const landings0 = accumulate.stats.reserved_landings.load();
    var half: [wt.BS / 2]u8 = undefined;
    fill(&half, 0x30);
    const o1 = try w.wp.write(io, .{ .ino = 30, .offset = 2 * wt.BS, .payload = .{ .bytes = &half }, .class = .shared, .file = World.striped(4 * wt.BS) });
    try testing.expectEqual(@as(u32, 1), o1.count(.accumulate_park));
    var rest: [wt.BS / 2]u8 = undefined;
    fill(&rest, 0x31);
    const o2 = try w.wp.write(io, .{ .ino = 30, .offset = 2 * wt.BS + wt.BS / 2, .payload = .{ .bytes = &rest }, .class = .shared, .file = World.striped(4 * wt.BS) });
    try testing.expectEqual(@as(u32, 1), o2.count(.write_through));
    w.wp.quiesce(io);
    try testing.expectEqual(landings0 + 1, accumulate.stats.reserved_landings.load());
    const written_2 = seam.lookup(30, 2).?;
    try testing.expect(!written_2.unwritten);
    try testing.expect(written_2.sameRef(reserved_2, wt.BS));
    try testing.expect(seam.reserved(30, 2) == null);
    try testing.expectEqual(used_a0 + used_b0 + 3, w.f.alloc_a.usedBlocks() + w.f.alloc_b.usedBlocks()); // no mint, no free
    try testing.expectEqual(@as(u64, 3), w.f.map.refs_added); // the flip is no reference move
    try testing.expectEqual(@as(u64, 0), w.f.map.refs_released);
    try testing.expectEqual(@as(usize, 0), w.f.frees.freed.items.len);
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(written_2.vol_tag, written_2.offset, &got);
    try testing.expectEqualSlices(u8, &half, got[0 .. wt.BS / 2]);
    try testing.expectEqualSlices(u8, &rest, got[wt.BS / 2 ..]);
    // The landed block is W1's now: a page rewrite patches in place.
    var page: [PAGE]u8 align(4096) = @splat(0x32);
    const o3 = try w.wp.write(io, .{ .ino = 30, .offset = 2 * wt.BS + PAGE, .payload = .{ .bytes = &page }, .class = .shared, .file = World.striped(4 * wt.BS) });
    try testing.expectEqual(@as(u32, 1), o3.count(.patch));
    // ZERO_RANGE over blocks 0–1: the written block 0 flips (no I/O, its
    // key purged), the reserved block 1 stands, nothing minted.
    const writes1 = w.f.dev.writes.items.len;
    const zeroed0 = stats.fallocate_blocks_zeroed.load();
    const b0 = seam.lookup(30, 0).?;
    const z = try w.wp.reserveRange(io, 30, 0, 1, true);
    try testing.expectEqual(@as(u32, 0), z.reserved);
    try testing.expectEqual(@as(u32, 1), z.zeroed);
    try testing.expectEqual(zeroed0 + 1, stats.fallocate_blocks_zeroed.load());
    try testing.expectEqual(writes1, w.f.dev.writes.items.len);
    try testing.expect(seam.lookup(30, 0) == null);
    try testing.expect(seam.reserved(30, 0).?.sameRef(b0, wt.BS));
    try testing.expectEqual(@as(u64, 3), w.f.map.refs_added);
    try testing.expectEqual(@as(u64, 0), w.f.map.refs_released);
    // A clone shares block 3's reservation: a write there finds the block
    // shared, declines the takeover and mints fresh; the publish displaces
    // the reservation (freed by the upload), the clone keeps its own.
    const res3 = seam.reserved(30, 3).?;
    const idx3 = w.f.alloc_a.idxOf(res3.offset);
    const on_a = res3.vol_tag == wt.VOL_A;
    const alloc3 = if (on_a) w.f.alloc_a else w.f.alloc_b;
    try testing.expect(alloc3.incrementRefcount(if (on_a) idx3 else w.f.alloc_b.idxOf(res3.offset)));
    const declines0 = accumulate.stats.reserved_landing_declines.load();
    var whole: [wt.BS]u8 align(4096) = undefined;
    fill(&whole, 0x33);
    const o4 = try w.wp.write(io, .{ .ino = 30, .offset = 3 * wt.BS, .payload = .{ .bytes = &whole }, .class = .shared, .file = World.striped(4 * wt.BS) });
    try testing.expectEqual(@as(u32, 1), o4.count(.write_through));
    w.wp.quiesce(io);
    try testing.expectEqual(declines0 + 1, accumulate.stats.reserved_landing_declines.load());
    // The fresh block rode the rewrite epoch with the reservation parked
    // as its displaced key; the close's save publishes and frees it.
    const drift0 = counters.tripwireCount("rewrite_shadow_displaced_drift");
    w.wp.flushIno(io, 30);
    try testing.expectEqual(drift0, counters.tripwireCount("rewrite_shadow_displaced_drift"));
    const b3 = seam.lookup(30, 3).?;
    try testing.expect(!b3.sameRef(res3, wt.BS));
    try testing.expectEqual(@as(u64, 1), w.f.map.refs_released); // the reservation displaced (the clone's share stands)
    try testing.expectEqual(@as(usize, 1), w.f.frees.freed.items.len);
    try testing.expect(w.f.frees.freed.items[0].sameRef(res3, wt.BS));
}

test "the fallocate reservation under a FULL store: a write inside the reservation is admitted (the custody probe reads the reserved block as the file's own) and lands in place where a hole's write is refused ENOSPC before any byte; a reservation the store cannot cover refuses ENOSPC with its chunk's mints returned and the earlier blocks standing" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, false);
    defer w.deinit(testing.allocator);
    try w.f.map.create(40, .striped, 4 * wt.BS);
    try w.f.map.create(41, .striped, 4 * wt.BS);
    _ = try w.wp.reserveRange(io, 40, 0, 1, false);
    // Exhaust both volumes: every remaining block minted away.
    while (w.f.alloc_a.allocateBlock()) |_| {} else |_| {}
    while (w.f.alloc_b.allocateBlock()) |_| {} else |_| {}
    var cands: [router.MAX_VOLUMES]*block_alloc.BlockAllocator = undefined;
    try testing.expect(enospc.freshSupplyExhausted(w.f.vols.candidates(&cands)));
    // A hole's write: refused before any byte (the terminal law).
    var half: [wt.BS / 2]u8 = @splat(0x40);
    const ref0 = stats.write_enospc_refusals.load();
    try testing.expectError(error.NoSpaceLeft, w.wp.write(io, .{ .ino = 41, .offset = 0, .payload = .{ .bytes = &half }, .class = .shared, .file = World.striped(4 * wt.BS) }));
    try testing.expectEqual(ref0 + 1, stats.write_enospc_refusals.load());
    // The reservation's write: admitted, parked, landed in place.
    const o1 = try w.wp.write(io, .{ .ino = 40, .offset = 0, .payload = .{ .bytes = &half }, .class = .shared, .file = World.striped(4 * wt.BS) });
    try testing.expectEqual(@as(u64, wt.BS / 2), o1.written);
    const o2 = try w.wp.write(io, .{ .ino = 40, .offset = wt.BS / 2, .payload = .{ .bytes = &half }, .class = .shared, .file = World.striped(4 * wt.BS) });
    try testing.expectEqual(@as(u32, 1), o2.count(.write_through));
    w.wp.quiesce(io);
    try testing.expect(w.f.map.seam().lookup(40, 0) != null);
    try testing.expect(!w.wp.table.has(40, 0));
    // A whole-block write into the second reserved block: the overlay is
    // off here, so the accumulation arm lands it in place too.
    var whole: [wt.BS]u8 align(4096) = @splat(0x41);
    const o3 = try w.wp.write(io, .{ .ino = 40, .offset = wt.BS, .payload = .{ .bytes = &whole }, .class = .shared, .file = World.striped(4 * wt.BS) });
    try testing.expectEqual(@as(u32, 1), o3.count(.write_through));
    try testing.expect(w.f.map.seam().lookup(40, 1) != null);
    // A reservation the store cannot cover: ENOSPC, nothing leaked.
    const used0 = w.f.alloc_a.usedBlocks() + w.f.alloc_b.usedBlocks();
    const added0 = w.f.map.refs_added;
    try testing.expectError(error.NoSpaceLeft, w.wp.reserveRange(io, 41, 0, 1, false));
    try testing.expectEqual(used0, w.f.alloc_a.usedBlocks() + w.f.alloc_b.usedBlocks());
    try testing.expectEqual(added0, w.f.map.refs_added);
    try testing.expect(w.f.map.seam().reserved(41, 0) == null);
    // Supply returns: the reservation lands.
    _ = try w.f.alloc_a.freeBlock(5);
    const r = try w.wp.reserveRange(io, 41, 0, 0, false);
    try testing.expectEqual(@as(u32, 1), r.reserved);
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

/// The pages `[off, off + len)` touches.
fn pagesTouched(off: u64, len: u64) u64 {
    return (std.mem.alignForward(u64, off + len, PAGE) - std.mem.alignBackward(u64, off, PAGE)) / PAGE;
}

test "the extending append's drain on a cache-less set (rung-10 dev perf §10.4: 1.6 GB of device writes for 36 KB of appends): stream-adjacent 72 B appends into a mapped sole-owner passthrough tail block, each drained at its close, land the TOUCHED pages in place — device bytes per drain = pages × P ≤ pages(u, 1) × P / u per appended byte, the partial pages' other bytes seeded back from the mapping (seed ≤ written), no whole-block upload, no map merge, no free, the binding unchanged; a page-aligned 4 KiB append lands as exactly its page with no seed" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, false);
    defer w.deinit(testing.allocator);
    const a = try w.f.publishBlock(44, 0, 0xAA);
    const used = w.f.alloc_a.usedBlocks();
    const merges = w.f.map.merges.items.len;
    const wt0 = accumulate.stats.write_through_bytes.load();
    const ip0 = accumulate.stats.drain_inplace_write_bytes.load();
    const seed0 = accumulate.stats.drain_inplace_seed_bytes.load();
    const drains0 = accumulate.stats.drain_inplace_drains.load();
    // EOF inside the tail block: the bytes past it are the old image's.
    const eof0: u64 = 10_000;
    var size = eof0;
    var line: [72]u8 = undefined;
    var device: u64 = 0;
    var i: u8 = 0;
    while (i < 60) : (i += 1) {
        fill(&line, i);
        w.f.dev.resetLedger();
        const o = try w.wp.write(io, .{ .ino = 44, .offset = size, .payload = .{ .bytes = &line }, .class = .entire_op, .file = World.striped(size) });
        try testing.expectEqual(@as(u64, line.len), o.written);
        w.wp.flushIno(io, 44); // the close's drain
        try testing.expect(!w.wp.table.has(44, 0));
        const pages = pagesTouched(size, line.len);
        try testing.expectEqual(pages * PAGE, w.f.dev.writeBytesTotal());
        try testing.expect(w.f.dev.read_bytes <= w.f.dev.writeBytesTotal());
        try testing.expect(w.f.dev.writeBytesTotal() * 1_000_000 / line.len <= extent.inPlaceBoundPpm(line.len, 1));
        if (i != 0) device += w.f.dev.writeBytesTotal();
        size += line.len;
    }
    try testing.expectEqual(wt0, accumulate.stats.write_through_bytes.load());
    try testing.expectEqual(used, w.f.alloc_a.usedBlocks());
    try testing.expectEqual(merges, w.f.map.merges.items.len);
    try testing.expectEqual(@as(usize, 0), w.f.frees.freed.items.len);
    try testing.expect(w.f.map.seam().lookup(44, 0).?.eql(a));
    try testing.expect(block_alloc.Incarnation.isStable(w.f.alloc_a.incarnationWord(w.f.alloc_a.idxOf(a.offset))));
    // The first append is not stream-adjacent (the W2 fold's arm); the 59
    // after it are the accumulation arm's, each drained in place: the
    // closure `drain_inplace_write_bytes ≡` their device bytes.
    try testing.expectEqual(drains0 + 59, accumulate.stats.drain_inplace_drains.load());
    try testing.expectEqual(device, accumulate.stats.drain_inplace_write_bytes.load() - ip0);
    try testing.expect(accumulate.stats.drain_inplace_seed_bytes.load() - seed0 <= device);
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(a.vol_tag, a.offset, &got);
    try testing.expect(std.mem.allEqual(u8, got[0..eof0], 0xAA));
    i = 0;
    while (i < 60) : (i += 1) {
        fill(&line, i);
        try testing.expectEqualSlices(u8, &line, got[eof0 + @as(u64, i) * line.len ..][0..line.len]);
    }
    try testing.expect(std.mem.allEqual(u8, got[size..], 0xAA));
    // Up to the page edge, then a page-aligned 4 KiB append: exactly its page, no seed.
    var pad: [PAGE]u8 = undefined;
    const to_edge = std.mem.alignForward(u64, size, PAGE) - size;
    fill(pad[0..to_edge], 0x61);
    _ = try w.wp.write(io, .{ .ino = 44, .offset = size, .payload = .{ .bytes = pad[0..to_edge] }, .class = .entire_op, .file = World.striped(size) });
    w.wp.flushIno(io, 44);
    size += to_edge;
    w.f.dev.resetLedger();
    fill(&pad, 0x62);
    _ = try w.wp.write(io, .{ .ino = 44, .offset = size, .payload = .{ .bytes = &pad }, .class = .entire_op, .file = World.striped(size) });
    w.wp.flushIno(io, 44);
    try testing.expectEqual(@as(u64, PAGE), w.f.dev.writeBytesTotal());
    try testing.expectEqual(@as(u64, 0), w.f.dev.read_bytes);
    w.f.dev.peek(a.vol_tag, a.offset, &got);
    try testing.expectEqualSlices(u8, &pad, got[size..][0..PAGE]);
    try testing.expect(w.f.map.seam().lookup(44, 0).?.eql(a));
}

test "a held-open appender's ONE drain lands its touched pages (rung-10 dev perf §10.4's fork appender in the control arm: 4.2 MB for 144 KB): the first append parks a W2 page overlay over the mapped tail (not stream-adjacent), the 39 stream-adjacent appends after it CONTINUE that overlay instead of escalating it (a whole-block seed read), and the close's drain folds the touched pages in place — device bytes = the touched pages, seed ≤ written, no whole-block upload, the binding unchanged, every byte exact" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, false);
    defer w.deinit(testing.allocator);
    const a = try w.f.publishBlock(46, 0, 0xAA);
    const wt0 = accumulate.stats.write_through_bytes.load();
    const eof0: u64 = 10_000;
    var size = eof0;
    var line: [72]u8 = undefined;
    var i: u8 = 0;
    while (i < 40) : (i += 1) {
        fill(&line, i);
        const o = try w.wp.write(io, .{ .ino = 46, .offset = size, .payload = .{ .bytes = &line }, .class = .entire_op, .file = World.striped(size) });
        try testing.expectEqual(@as(u32, 1), o.count(.extent));
        size += line.len;
    }
    try testing.expectEqual(@as(u64, 0), w.f.dev.read_bytes); // nothing seeded at a write
    w.wp.flushIno(io, 46); // the close's drain
    try testing.expect(!w.wp.table.has(46, 0));
    try testing.expectEqual(pagesTouched(eof0, size - eof0) * PAGE, w.f.dev.writeBytesTotal());
    try testing.expect(w.f.dev.read_bytes <= w.f.dev.writeBytesTotal());
    try testing.expectEqual(wt0, accumulate.stats.write_through_bytes.load());
    try testing.expect(w.f.map.seam().lookup(46, 0).?.eql(a));
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(a.vol_tag, a.offset, &got);
    try testing.expect(std.mem.allEqual(u8, got[0..eof0], 0xAA));
    i = 0;
    while (i < 40) : (i += 1) {
        fill(&line, i);
        try testing.expectEqualSlices(u8, &line, got[eof0 + @as(u64, i) * line.len ..][0..line.len]);
    }
    try testing.expect(std.mem.allEqual(u8, got[size..], 0xAA));
}

test "the in-place drain's crash law (the kill rows): a drain killed at every grain of its DMA leaves the file's old bytes intact and each appended byte old or new — never a torn old byte, never a recycled pool byte on the device; the entry stays parked and the next drain lands it (never lossy)" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, false);
    defer w.deinit(testing.allocator);
    const a = try w.f.publishBlock(45, 0, 0xAA);
    // A first append (the W2 arm) makes the next one stream-adjacent.
    const eof0: u64 = 3 * PAGE + 1000;
    var head: [100]u8 = undefined;
    fill(&head, 0x11);
    _ = try w.wp.write(io, .{ .ino = 45, .offset = eof0, .payload = .{ .bytes = &head }, .class = .entire_op, .file = World.striped(eof0) });
    w.wp.flushIno(io, 45);
    const eof = eof0 + head.len;
    var before: [wt.BS]u8 = undefined;
    w.f.dev.peek(a.vol_tag, a.offset, &before);
    // A 5,000 B append from inside page 3 spans pages 3 and 4: ONE DMA of two grains.
    var body: [5000]u8 = undefined;
    fill(&body, 0x22);
    try testing.expectEqual(@as(u64, 2), pagesTouched(eof, body.len));
    var kill_at: usize = 0;
    while (kill_at < 2) : (kill_at += 1) {
        _ = w.wp.last_write_end.note(45, eof, 0); // the append is stream-adjacent
        const o = try w.wp.write(io, .{ .ino = 45, .offset = eof, .payload = .{ .bytes = &body }, .class = .entire_op, .file = World.striped(eof) });
        try testing.expectEqual(@as(u32, 1), o.count(.accumulate_park));
        log.testing.expectErrorLogs(1);
        w.f.dev.fail_next = error.InputOutput;
        w.f.dev.torn_grains = kill_at;
        w.wp.flushIno(io, 45);
        try log.testing.expectAllFired();
        try testing.expect(w.wp.table.has(45, 0)); // never lossy: still parked
        var crash: [wt.BS]u8 = undefined;
        w.f.dev.peek(a.vol_tag, a.offset, &crash);
        try testing.expectEqualSlices(u8, before[0..eof], crash[0..eof]);
        for (crash[eof..][0..body.len], before[eof..][0..body.len], body) |c, old, new| try testing.expect(c == old or c == new);
        try testing.expectEqualSlices(u8, before[eof + body.len ..], crash[eof + body.len ..]);
        // The next drain lands it.
        w.wp.flushIno(io, 45);
        try testing.expect(!w.wp.table.has(45, 0));
        w.f.dev.peek(a.vol_tag, a.offset, &crash);
        try testing.expectEqualSlices(u8, before[0..eof], crash[0..eof]);
        try testing.expectEqualSlices(u8, &body, crash[eof..][0..body.len]);
        try testing.expectEqualSlices(u8, before[eof + body.len ..], crash[eof + body.len ..]);
        // Reset the tail for the next kill point (a non-adjacent rewrite of the old bytes).
        _ = w.wp.last_write_end.note(45, 0, 0);
        _ = try w.wp.write(io, .{ .ino = 45, .offset = eof, .payload = .{ .bytes = before[eof..][0..body.len] }, .class = .shared, .file = World.striped(eof + body.len) });
        w.wp.flushIno(io, 45);
        w.f.dev.peek(a.vol_tag, a.offset, &crash);
        try testing.expectEqualSlices(u8, &before, &crash);
    }
    try testing.expect(w.f.map.seam().lookup(45, 0).?.eql(a));
}

/// fio's random rewriter's block under the timer (the perf rows of
/// `rung-10-writeback-timer.md`): a rewrite that happens to be
/// stream-adjacent parks a DEFERRED full buffer (arm G; W1 declines every
/// later write of the block on its RAM custody), and the run table's
/// overflow seeds its complement whole (`NeedsSeed`) — a CONTENT-VALID
/// buffer over the mapping. Returns the user bytes written.
fn parkSeededRewrites(w: *WritePath, io: Io, ino: u64) !u64 {
    const file = World.striped(wt.BS);
    var page: [PAGE]u8 = undefined;
    fill(&page, 0x10);
    _ = w.last_write_end.note(ino, 0, PAGE);
    const o = try w.write(io, .{ .ino = ino, .offset = PAGE, .payload = .{ .bytes = &page }, .class = .shared, .file = file });
    try testing.expectEqual(@as(u32, 1), o.count(.accumulate_park));
    var user: u64 = PAGE;
    // 65 disjoint 16 B pieces in page 3 overflow the 65-run table.
    var piece: [16]u8 = undefined;
    var i: u32 = 0;
    while (i < cov.CoverageUnion.max_runs) : (i += 1) {
        fill(&piece, @truncate(0x40 + i));
        _ = try w.write(io, .{ .ino = ino, .offset = 3 * PAGE + 32 * i, .payload = .{ .bytes = &piece }, .class = .shared, .file = file });
        user += piece.len;
    }
    const e = w.table.get(ino, 0).?;
    try testing.expect(e.content_valid and e.clean_base != null);
    return user;
}

test "random 4 KiB rewrites under the write-back timer (decision 0003 §AJ.7): a CONTENT-VALID buffer seeded over its sole-owner mapping drains its DIRTY pages in place — device = pages(dirty) × P ≤ Σ pages(w) × P (≤ the user bytes for page-aligned rewrites: device ÷ user ≤ 1), never the whole block; no mint, no merge, no epoch shadow, the binding unchanged, every byte exact; the block is custody-free after (W1 takes the next rewrite)" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, false);
    defer w.deinit(testing.allocator);
    const a = try w.f.publishBlock(47, 0, 0xAA);
    const file = World.striped(wt.BS);
    var user = try parkSeededRewrites(w.wp, io, 47);
    var writes: u64 = 1 + cov.CoverageUnion.max_runs;
    var page: [PAGE]u8 = undefined;
    for ([_]u32{ 8, 11, 14 }) |p| {
        fill(&page, @truncate(0x80 + p));
        const o = try w.wp.write(io, .{ .ino = 47, .offset = @as(u64, p) * PAGE, .payload = .{ .bytes = &page }, .class = .shared, .file = file });
        try testing.expectEqual(@as(u32, 1), o.count(.accumulate_park));
        user += PAGE;
        writes += 1;
    }
    var expect: [wt.BS]u8 = undefined;
    @memcpy(&expect, w.wp.table.get(47, 0).?.repr.full.data[0..wt.BS]);
    w.f.dev.resetLedger();
    const used = w.f.alloc_a.usedBlocks();
    const merges = w.f.map.merges.items.len;
    const wt0 = accumulate.stats.write_through_bytes.load();
    const dd0 = accumulate.stats.drain_inplace_dirty_drains.load();
    const shadow0 = epoch_mod.stats.rewrite_shadow_bytes.load();
    const drained = w.wp.drainAged(io, 47, clock.monotonicNs());
    try testing.expect(drained.landed and drained.kept_born_ns == null);
    try testing.expect(!w.wp.table.has(47, 0));
    // The dirty pages: 1, 3, 8, 11, 14 — five DMAs of a page each.
    try testing.expectEqual(@as(u64, 5 * PAGE), w.f.dev.writeBytesTotal());
    try testing.expect(w.f.dev.writeBytesTotal() <= writes * PAGE);
    try testing.expectEqual(dd0 + 1, accumulate.stats.drain_inplace_dirty_drains.load());
    try testing.expectEqual(wt0, accumulate.stats.write_through_bytes.load());
    try testing.expectEqual(shadow0, epoch_mod.stats.rewrite_shadow_bytes.load());
    try testing.expectEqual(used, w.f.alloc_a.usedBlocks());
    try testing.expectEqual(merges, w.f.map.merges.items.len);
    try testing.expect(w.f.map.seam().lookup(47, 0).?.eql(a));
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(a.vol_tag, a.offset, &got);
    try testing.expectEqualSlices(u8, &expect, &got);
    // Custody-free: the next page-aligned rewrite is W1's, 1:1.
    fill(&page, 0x99);
    const o = try w.wp.write(io, .{ .ino = 47, .offset = 6 * PAGE, .payload = .{ .bytes = &page }, .class = .shared, .file = file });
    try testing.expectEqual(@as(u32, 1), o.count(.patch));
    try testing.expectEqual(@as(u64, 6 * PAGE), w.f.dev.writeBytesTotal());
}

test "a superseded upload's snapshot publishes and its successor drains only what changed after it (decision 0003 §AJ.7): a write landing during the upload's DMA supersedes it; under the timer's cutoff the snapshot publishes (`write_pipeline_superseded_publishes`, no mint abandoned) and the entry stays parked with the newer page dirty over the published key — reads keep the entry (never the older image); once the epoch saves, the next pass lands that ONE page in place over the published key" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, false);
    defer w.deinit(testing.allocator);
    _ = try w.f.publishBlock(48, 0, 0xAA);
    _ = try parkSeededRewrites(w.wp, io, 48);
    // The racing writer: page 9 lands during the upload's DMA.
    const Racer = struct {
        var wp: *WritePath = undefined;
        var fired: bool = false;
        fn hook(_: *anyopaque, _: *wt.FakeDevice, _: u64, _: u64, _: u64) void {
            if (fired) return;
            fired = true;
            var newer: [PAGE]u8 = undefined;
            fill(&newer, 0x5A);
            const o = wp.write(testing.io, .{ .ino = 48, .offset = 9 * PAGE, .payload = .{ .bytes = &newer }, .class = .shared, .file = World.striped(wt.BS) }) catch unreachable;
            assert(o.count(.accumulate_park) == 1);
        }
    };
    Racer.wp = w.wp;
    Racer.fired = false;
    var dummy: u8 = 0;
    w.f.dev.hook = .{ .ctx = &dummy, .call = &Racer.hook };
    defer w.f.dev.hook = null;
    const sp0 = pipeline.stats.write_pipeline_superseded_publishes.load();
    const abandons0 = block_alloc.stats.unpublished_mint_abandons.load();
    var prof = pipeline.Prof.start();
    try testing.expectEqual(accumulate.Disposition.published, accumulate.uploadParkedBlock(&w.wp.table, w.wp.uploadDeps(), io, 48, 0, 0, &prof));
    prof.finish();
    w.f.dev.hook = null;
    try testing.expect(Racer.fired);
    try testing.expectEqual(sp0 + 1, pipeline.stats.write_pipeline_superseded_publishes.load());
    try testing.expectEqual(abandons0, block_alloc.stats.unpublished_mint_abandons.load());
    // Kept: page 9 dirty alone, over the snapshot's key.
    const e = w.wp.table.get(48, 0).?;
    const snap_key = e.clean_base.?;
    try testing.expectEqual(@as(usize, 1), e.dirty.count());
    try testing.expect(e.dirty.isSet(9));
    var newest: [wt.BS]u8 = undefined;
    @memcpy(&newest, e.repr.full.data[0..wt.BS]);
    var page9: [PAGE]u8 = undefined;
    fill(&page9, 0x5A);
    try testing.expectEqualSlices(u8, &page9, newest[9 * PAGE ..][0..PAGE]);
    // The published snapshot is older; the read seam serves the entry.
    var img: [wt.BS]u8 = undefined;
    w.f.dev.peek(snap_key.vol_tag, snap_key.offset, &img);
    try testing.expect(std.mem.allEqual(u8, img[9 * PAGE ..][0..PAGE], 0xAA));
    var out: [PAGE]u8 = undefined;
    try testing.expect(w.wp.writeSideView().serve(io, .{ .ino = 48, .block = 0, .rel = 9 * PAGE, .out = &out }) == .served);
    try testing.expectEqualSlices(u8, &page9, &out);
    // A pass with nothing aged closes the epoch (map → the snapshot's key).
    _ = w.wp.drainAged(io, 48, 0);
    try testing.expect(w.f.map.seam().lookup(48, 0).?.eql(snap_key));
    try testing.expect(w.wp.table.has(48, 0));
    // The successor: one page, in place.
    w.f.dev.resetLedger();
    const drained = w.wp.drainAged(io, 48, clock.monotonicNs());
    try testing.expect(drained.landed and drained.kept_born_ns == null);
    try testing.expect(!w.wp.table.has(48, 0));
    try testing.expectEqual(@as(u64, PAGE), w.f.dev.writeBytesTotal());
    try testing.expect(w.f.map.seam().lookup(48, 0).?.eql(snap_key));
    w.f.dev.peek(snap_key.vol_tag, snap_key.offset, &img);
    try testing.expectEqualSlices(u8, &newest, &img);
}

test "the in-place dirty drain's crash law (the kill rows, decision 0003 §AJ.7): a drain cut after any of its DMAs leaves every acked write whole or absent — each dirty run one DMA from the cell — and every clean page old; a DMA torn at any page leaves each page old or new, never a torn page; the entry stays parked and the next drain lands every byte (never lossy)" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, false);
    defer w.deinit(testing.allocator);
    const a = try w.f.publishBlock(49, 0, 0xAA);
    const file = World.striped(wt.BS);
    // Dirty runs: page 1, page 3 (the seeded pieces), pages 11–12 (one 8 KiB write) — three DMAs.
    const Cut = struct {
        var dev: *wt.FakeDevice = undefined;
        var after: usize = 0;
        var torn: usize = 0;
        var seen: usize = 0;
        fn hook(_: *anyopaque, d: *wt.FakeDevice, _: u64, _: u64, _: u64) void {
            seen += 1;
            if (seen == after) {
                d.fail_next = error.InputOutput;
                d.torn_grains = torn;
            }
        }
    };
    var before: [wt.BS]u8 = undefined;
    var pair: [2 * PAGE]u8 = undefined;
    var round: u8 = 0;
    const cuts = [_][2]usize{ .{ 0, 0 }, .{ 1, 0 }, .{ 2, 0 }, .{ 2, 1 } };
    for (cuts) |cut| {
        round += 1;
        w.f.dev.peek(a.vol_tag, a.offset, &before);
        _ = try parkSeededRewrites(w.wp, io, 49);
        fill(&pair, 0xC0 +% round);
        _ = try w.wp.write(io, .{ .ino = 49, .offset = 11 * PAGE, .payload = .{ .bytes = &pair }, .class = .shared, .file = file });
        var after: [wt.BS]u8 = undefined;
        @memcpy(&after, w.wp.table.get(49, 0).?.repr.full.data[0..wt.BS]);
        log.testing.expectErrorLogs(1);
        Cut.dev = &w.f.dev;
        Cut.after = cut[0];
        Cut.torn = cut[1];
        Cut.seen = 0;
        var dummy: u8 = 0;
        if (cut[0] == 0) {
            w.f.dev.fail_next = error.InputOutput;
        } else w.f.dev.hook = .{ .ctx = &dummy, .call = &Cut.hook };
        _ = w.wp.drainAged(io, 49, clock.monotonicNs());
        w.f.dev.hook = null;
        try log.testing.expectAllFired();
        try testing.expect(w.wp.table.has(49, 0)); // never lossy: still parked
        var crash: [wt.BS]u8 = undefined;
        w.f.dev.peek(a.vol_tag, a.offset, &crash);
        var p: usize = 0;
        while (p < wt.BS / PAGE) : (p += 1) {
            const c = crash[p * PAGE ..][0..PAGE];
            const old = std.mem.eql(u8, c, before[p * PAGE ..][0..PAGE]);
            const new = std.mem.eql(u8, c, after[p * PAGE ..][0..PAGE]);
            try testing.expect(old or new); // never a torn page
        }
        // The 8 KiB write is whole or absent unless its own DMA was torn.
        const w_old = std.mem.eql(u8, crash[11 * PAGE ..][0 .. 2 * PAGE], before[11 * PAGE ..][0 .. 2 * PAGE]);
        const w_new = std.mem.eql(u8, crash[11 * PAGE ..][0 .. 2 * PAGE], &pair);
        if (cut[1] == 0) try testing.expect(w_old or w_new);
        // The next drain lands every byte.
        _ = w.wp.drainAged(io, 49, clock.monotonicNs());
        try testing.expect(!w.wp.table.has(49, 0));
        w.f.dev.peek(a.vol_tag, a.offset, &crash);
        try testing.expectEqualSlices(u8, &after, &crash);
        try testing.expect(w.f.map.seam().lookup(49, 0).?.eql(a));
    }
}

/// The bytes a recycled block's previous owner left on the device.
const STALE: u8 = 0xAA;

/// `ino`'s block 0 re-minted from a block a previous owner filled with
/// `STALE`, holding a promoted sub-block image: the own-block DMA landed the
/// file's `eof` bytes grain-padded with zeros (`plane.ownBlockStored`), the
/// tail past the grain is the previous owner's. `shared` marks it cloned;
/// `transformed` mints it on the fixture's transformed volume (no image
/// seam bound: its seeds read the stored bytes whole, the worst case of a
/// framed image whose plaintext covers less than the block).
fn recycledEofBlock(w: *World, ino: u64, eof: u64, shared: bool, transformed: bool) !Binding {
    assert(eof <= PAGE);
    const b = if (transformed) blk: {
        const m = try w.f.alloc_b.allocateBlock();
        var buf: [wt.BS]u8 = @splat(STALE);
        try w.f.dev.seam().vtable.write(&w.f.dev, wt.VOL_B, m.offset, &buf);
        w.f.alloc_b.publishBlock(m.idx);
        const tb = Binding.wholeBlock(wt.VOL_B, m.offset, m.stamp);
        try w.f.map.create(ino, .striped, eof);
        try w.f.map.bind(ino, 0, tb);
        break :blk tb;
    } else try w.f.publishBlock(ino, 0, STALE);
    var image: [PAGE]u8 align(PAGE) = @splat(0);
    fill(image[0..@intCast(eof)], 0x31);
    try w.f.dev.seam().vtable.write(&w.f.dev, b.vol_tag, b.offset, &image);
    w.f.map.files.getPtr(ino).?.size = eof;
    if (shared) w.f.alloc_a.markShared(w.f.alloc_a.idxOf(b.offset));
    w.f.dev.resetLedger();
    return b;
}

/// `ino`'s `block` as a reader sees it now: the write side's window (its
/// covered runs, the gaps from the base it names), else the map's block,
/// else a hole's zeros.
fn readBlockNow(w: *World, io: Io, ino: u64, block: u32, out: []u8) !void {
    @memset(out, 0);
    switch (w.wp.writeSideView().serve(io, .{ .ino = ino, .block = block, .rel = 0, .out = out })) {
        .none => if (w.f.map.seam().lookup(ino, block)) |b| w.f.dev.peek(b.vol_tag, b.offset, out),
        .partial => |p| if (p.base) |base| {
            var img: [wt.BS]u8 = undefined;
            w.f.dev.peek(base.vol_tag, base.offset, &img);
            var gaps = p.covered.gaps(0, wt.BS);
            while (gaps.next()) |gp| @memcpy(out[gp.start..gp.end], img[gp.start..gp.end]);
        },
        .redirect => |shadow| w.f.dev.peek(shadow.vol_tag, shadow.offset, out),
        .served => {},
        .settle, .busy => return error.TestUnexpectedResult,
    }
}

fn expectZeros(what: []const u8, bytes: []const u8, base: u64) !void {
    if (std.mem.indexOfNone(u8, bytes, &.{0})) |i| {
        std.debug.print("{s}: byte {d} reads 0x{x}, not 0 (the previous owner's)\n", .{ what, base + i, bytes[i] });
        return error.TestUnexpectedResult;
    }
}

test "decision 0003 §AL — a write past EOF never exposes the bytes a recycled block's previous owner left: a promoted sub-block image (its EOF bytes landed, the tail the previous owner's marker) written past its EOF reads zeros across [EOF, write) on every arm — the W2 page overlay folded in place, the accumulation drained in place, the overlay arm's fresh landing, the seeded CoW upload of a shared block and of a transformed volume's block, a write into a LATER block (the EOF block's tail is the gap) — live through the write side's view and on the device after the drain" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, true);
    defer w.deinit(testing.allocator);
    const Case = struct { ino: u64, eof: u64 = 100, offset: u64, len: u64, arm: Arm, shared: bool = false, transformed: bool = false };
    const cases = [_]Case{
        // Below the W2 RAM trigger (`pages ≥ B/4P`) with the gap's pages.
        .{ .ino = 70, .offset = 2 * PAGE + 7, .len = 10, .arm = .extent },
        .{ .ino = 71, .offset = 5 * PAGE + 3, .len = 5 * PAGE, .arm = .accumulate_park },
        .{ .ino = 72, .eof = PAGE, .offset = 8 * PAGE, .len = 3 * PAGE, .arm = .overlay },
        .{ .ino = 73, .offset = 5 * PAGE + 3, .len = 5 * PAGE, .arm = .accumulate_park, .shared = true },
        .{ .ino = 74, .offset = wt.BS + 10, .len = 10, .arm = .extent },
        .{ .ino = 75, .offset = 5 * PAGE + 3, .len = 10, .arm = .accumulate_park, .transformed = true },
    };
    var payload: [5 * PAGE]u8 = undefined;
    var own: [PAGE]u8 = undefined;
    var got: [wt.BS]u8 = undefined;
    for (cases) |c| {
        _ = try recycledEofBlock(w, c.ino, c.eof, c.shared, c.transformed);
        fill(own[0..@intCast(c.eof)], 0x31);
        fill(payload[0..@intCast(c.len)], @truncate(c.ino));
        const o = try w.wp.write(io, .{ .ino = c.ino, .offset = c.offset, .payload = .{ .bytes = payload[0..@intCast(c.len)] }, .class = .entire_op, .file = World.striped(c.eof) });
        try testing.expectEqual(@as(u32, 1), o.count(c.arm));
        const gap_end: usize = @intCast(@min(c.offset, wt.BS));
        for ([_]bool{ false, true }) |drained| {
            if (drained) {
                w.wp.flushIno(io, c.ino);
                w.wp.quiesce(io);
                try testing.expect(!w.wp.table.has(c.ino, 0));
            }
            try readBlockNow(w, io, c.ino, 0, &got);
            try testing.expectEqualSlices(u8, own[0..@intCast(c.eof)], got[0..@intCast(c.eof)]);
            try expectZeros(if (drained) "the gap on the device after the drain" else "the gap through the write side's view", got[@intCast(c.eof)..gap_end], c.eof);
            if (c.offset < wt.BS) try testing.expectEqualSlices(u8, payload[0..@intCast(c.len)], got[@intCast(c.offset)..][0..@intCast(c.len)]);
        }
    }
}

test "decision 0003 §AL — a truncate-up past a recycled tail reads zeros: `zeroExtension` lands [EOF, min(new size, block end)) of a mapped EOF block as the write path's own zeros (the extension's pages, nothing else), and declines a hole's EOF block and a block-aligned EOF (nothing to expose, nothing written)" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, false);
    defer w.deinit(testing.allocator);
    var got: [wt.BS]u8 = undefined;
    const b = try recycledEofBlock(w, 80, 100, false, false);
    const z0 = stats.write_gap_zero_bytes.load();
    try w.wp.zeroExtension(io, 80, World.striped(100), 6 * PAGE + 1);
    try readBlockNow(w, io, 80, 0, &got);
    try expectZeros("the grown range through the write side's view", got[100 .. 6 * PAGE + 1], 100);
    try testing.expectEqual(@as(u64, 6 * PAGE + 1 - 100), stats.write_gap_zero_bytes.load() - z0);
    w.f.dev.resetLedger();
    w.wp.flushIno(io, 80);
    // In place: the extension's seven pages, the binding unchanged.
    try testing.expectEqual(@as(u64, 7 * PAGE), w.f.dev.writeBytesTotal());
    try testing.expect(w.f.map.seam().lookup(80, 0).?.eql(b));
    w.f.dev.peek(b.vol_tag, b.offset, &got);
    try expectZeros("the grown range on the device", got[100 .. 6 * PAGE + 1], 100);
    // A hole's EOF block and a block-aligned EOF: nothing written.
    try w.f.map.create(81, .striped, 100);
    try w.wp.zeroExtension(io, 81, World.striped(100), wt.BS);
    try w.wp.zeroExtension(io, 80, World.striped(wt.BS), 3 * wt.BS);
    try testing.expectEqual(@as(u64, 6 * PAGE + 1 - 100), stats.write_gap_zero_bytes.load() - z0);
    try testing.expect(!w.wp.table.has(81, 0));
}

test "decision 0003 §AL.7 — a shrink's leftover is the extension law's: a block full of the file's OWN bytes whose size was clipped to mid-block with NO write (the shrink writes nothing) reads zeros past the clip on the next grow, whatever grows it — a truncate-up (`zeroExtension`), a write past EOF (the gap), an append at EOF followed by a grow (the bytes, then zeros) — live and on the device; nothing was owed at the shrink" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, false);
    defer w.deinit(testing.allocator);
    var got: [wt.BS]u8 = undefined;
    var own: [wt.BS]u8 = undefined;
    fill(&own, 0x51);
    const Grow = enum { truncate_up, write_past_eof, append_then_grow };
    for ([_]Grow{ .truncate_up, .write_past_eof, .append_then_grow }, 90..) |grow, ino| {
        // The file's own bytes fill the block; the shrink clips the size to
        // 100 and writes nothing.
        const b = try w.f.publishBlock(ino, 0, 0x51);
        try w.f.dev.seam().vtable.write(&w.f.dev, b.vol_tag, b.offset, &own);
        const clip: u64 = 100;
        w.f.map.files.getPtr(ino).?.size = clip;
        const z0 = stats.write_gap_zero_bytes.load();
        var payload: [PAGE]u8 = undefined;
        fill(&payload, 0x77);
        var eof: u64 = clip;
        var gap_end: u64 = undefined;
        switch (grow) {
            .truncate_up => {
                gap_end = 6 * PAGE + 1;
                try w.wp.zeroExtension(io, ino, World.striped(clip), gap_end);
            },
            .write_past_eof => {
                gap_end = 5 * PAGE + 3;
                _ = try w.wp.write(io, .{ .ino = ino, .offset = gap_end, .payload = .{ .bytes = &payload }, .class = .entire_op, .file = World.striped(clip) });
            },
            .append_then_grow => {
                // The append continues the file; its own bytes beyond stay
                // past EOF, and the grow zeroes from the NEW EOF.
                _ = try w.wp.write(io, .{ .ino = ino, .offset = clip, .payload = .{ .bytes = payload[0..50] }, .class = .entire_op, .file = World.striped(clip) });
                eof = clip + 50;
                w.f.map.files.getPtr(ino).?.size = eof;
                gap_end = 4 * PAGE;
                try w.wp.zeroExtension(io, ino, World.striped(eof), gap_end);
            },
        }
        try testing.expectEqual(gap_end - eof, stats.write_gap_zero_bytes.load() - z0);
        for ([_]bool{ false, true }) |drained| {
            if (drained) {
                w.wp.flushIno(io, ino);
                w.wp.quiesce(io);
            }
            try readBlockNow(w, io, ino, 0, &got);
            try testing.expectEqualSlices(u8, own[0..clip], got[0..clip]);
            if (grow == .append_then_grow) try testing.expectEqualSlices(u8, payload[0..50], got[clip..eof]);
            try expectZeros(if (drained) "the grown range on the device" else "the grown range through the write side's view", got[@intCast(eof)..@intCast(gap_end)], eof);
            if (grow == .write_past_eof) try testing.expectEqualSlices(u8, &payload, got[@intCast(gap_end)..][0..PAGE]);
        }
    }
}

test "the D3 fold seam (defrag --fold): `foldTargets` names the inos holding parked W2 extents (ascending, deduped — a full-buffer park is not a target), `foldParked` kicks each through the EXISTING fold — sole-owner passthrough blocks land in place at the page grain and retire, device ÷ user ≤ the §J in-place bound, the bindings unchanged, bytes exact; nothing left parked as an extent; a second census is empty" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, false);
    defer w.deinit(testing.allocator);
    const gpa = testing.allocator;
    // Three files: 51 (two blocks, a lone 512 B write in each), 52 (one),
    // 50 (a HALF-block accumulation park: a full buffer, never a D3 target).
    for ([_]u64{ 51, 52 }) |ino| {
        _ = try w.f.publishBlock(ino, 0, 0xAA);
        _ = try w.f.publishBlock(ino, 1, 0xBB);
    }
    try w.f.map.create(50, .striped, 0);
    var half: [wt.BS / 2 + 100]u8 = undefined;
    fill(&half, 3);
    const o = try w.wp.write(io, .{ .ino = 50, .offset = 0, .payload = .{ .bytes = &half }, .class = .entire_op, .file = World.striped(0) });
    try testing.expectEqual(@as(u32, 1), o.count(.accumulate_park));
    var sector: [512]u8 = undefined;
    const file = World.striped(2 * wt.BS);
    const parks = [_]struct { ino: u64, block: u64, page: u64 }{ .{ .ino = 52, .block = 0, .page = 3 }, .{ .ino = 51, .block = 1, .page = 6 }, .{ .ino = 51, .block = 0, .page = 2 } };
    for (parks, 0..) |p, i| {
        fill(&sector, @intCast(0x30 + i));
        const r = try w.wp.write(io, .{ .ino = p.ino, .offset = p.block * wt.BS + p.page * PAGE + 512, .payload = .{ .bytes = &sector }, .class = .shared, .file = file });
        try testing.expectEqual(@as(u32, 1), r.count(.extent));
        try testing.expect(w.wp.table.has(p.ino, @intCast(p.block)));
    }
    const gauge_parked = extent.stats.parked_extent_bytes.load();
    try testing.expect(gauge_parked >= 3 * PAGE);
    // The census: 51 and 52, ascending; 50's full buffer is not an extent.
    const targets = try w.wp.foldTargets(gpa);
    defer gpa.free(targets);
    try testing.expectEqualSlices(u64, &.{ 51, 52 }, targets);
    const used = w.f.alloc_a.usedBlocks();
    const merges = w.f.map.merges.items.len;
    w.f.dev.resetLedger();
    const passes0 = extent.stats.fold_inplace_passes.load();
    var folded: u32 = 0;
    for (targets) |ino| folded += w.wp.foldParked(io, ino);
    try testing.expectEqual(@as(u32, 3), folded);
    try testing.expectEqual(passes0 + 3, extent.stats.fold_inplace_passes.load());
    for (parks) |p| try testing.expect(!w.wp.table.has(p.ino, @intCast(p.block)));
    try testing.expect(w.wp.table.has(50, 0)); // the full buffer stays parked (its flush is the release's)
    try testing.expect(extent.stats.parked_extent_bytes.load() + 3 * PAGE <= gauge_parked);
    // The §J bound: 3 × 512 B of user bytes landed as 3 page DMAs (8×).
    try testing.expectEqual(@as(u64, 3 * PAGE), w.f.dev.writeBytesTotal());
    try testing.expect(w.f.dev.writeBytesTotal() * 1_000_000 / (3 * 512) <= extent.inPlaceBoundPpm(512, 512));
    try testing.expectEqual(used, w.f.alloc_a.usedBlocks());
    try testing.expectEqual(merges, w.f.map.merges.items.len);
    var got: [wt.BS]u8 = undefined;
    for (parks, 0..) |p, i| {
        const b = w.f.map.seam().lookup(p.ino, @intCast(p.block)).?;
        w.f.dev.peek(b.vol_tag, b.offset, &got);
        fill(&sector, @intCast(0x30 + i));
        try testing.expectEqualSlices(u8, &sector, got[p.page * PAGE + 512 .. p.page * PAGE + 1024]);
        try testing.expect(std.mem.allEqual(u8, got[p.page * PAGE .. p.page * PAGE + 512], if (p.block == 0) 0xAA else 0xBB));
    }
    const again = try w.wp.foldTargets(gpa);
    defer gpa.free(again);
    try testing.expectEqual(@as(usize, 0), again.len);
    // A kick on an ino with nothing parked folds nothing.
    try testing.expectEqual(@as(u32, 0), w.wp.foldParked(io, 51));
    w.wp.flushIno(io, 50);
}

test "W2 amplification law, CoW bases land ONCE per parked episode: a cloned (pinned) block declines the in-place arm, escalates seeded (one whole read) and PARKS — later small writes are memcpys and the release lands once: its dirty pages in place over the seeded base once the clone is gone (§AJ.7); a transformed volume's block takes the seeded checkout (one decode) and lands once, whole" {
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
    // The clone goes away: the block is the file's alone again, so the
    // parked buffer's ONE landing is its dirty pages over the seeded base.
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
    // ONE landing: the dirty pages 0–7 and 9, in place.
    try testing.expectEqual(@as(u64, 9 * PAGE), w.f.dev.writeBytesTotal());
    const b = w.f.map.seam().lookup(42, 0).?;
    try testing.expect(b.eql(a));
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

test "fsync and the RELEASE drain land EVERY block the ino parked before them — past one buffer of the flush's census (a random writer under the write-back timer parks a partial block per block it touched): 80 parked partial blocks, none left in RAM" {
    const io = testing.io;
    const blocks: u32 = 80;
    // Two inos' blocks parked at once, then landed on fresh blocks.
    const w = try World.initSized(testing.allocator, .sync_inline, true, 2 * blocks, 2 * blocks);
    defer w.deinit(testing.allocator);
    var tail: [1000]u8 = @splat(0x31);
    for ([_]u64{ 30, 31 }) |ino| {
        for (0..blocks) |b| _ = try w.wp.write(io, .{ .ino = ino, .offset = b * wt.BS, .payload = .{ .bytes = &tail }, .class = .entire_op, .file = World.striped(blocks * wt.BS) });
        var held: [2 * blocks]u32 = undefined;
        try testing.expectEqual(@as(usize, blocks), w.wp.table.blocksOf(ino, &held).len);
    }
    var prof = fsync_mod.Prof.start();
    try fsync_mod.fsync(w.wp.fsyncCtx(), io, 30, &prof);
    prof.finish();
    try testing.expect(!w.wp.table.holdsIno(30));
    w.wp.flushIno(io, 31);
    try testing.expect(!w.wp.table.holdsIno(31));
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

test "the no-open FLUSH's try-only probe (decision 0003 §AF): `holdsCustody` is true exactly while `flushIno` has something to land — a parked partial block, an open overlay record, an epoch the overlay fed — for that ino alone; after the drain every arm reads false" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, true);
    defer w.deinit(testing.allocator);
    try testing.expect(!w.wp.holdsCustody(30));
    _ = try w.f.publishBlock(30, 0, 0xAA);
    _ = try w.f.publishBlock(31, 0, 0xBB);
    try testing.expect(!w.wp.holdsCustody(30));
    // An overwrite half-block: an open overlay record (no parked block).
    var half: [wt.BS / 2]u8 align(4096) = @splat(0x30);
    _ = try w.wp.write(io, .{ .ino = 31, .offset = 0, .payload = .{ .bytes = &half }, .class = .shared, .file = World.striped(wt.BS) });
    try testing.expect(w.wp.overlay.anyOpenFast());
    try testing.expectEqual(@as(u32, 0), w.wp.table.parkedCount());
    try testing.expect(w.wp.holdsCustody(31));
    try testing.expect(!w.wp.holdsCustody(30));
    // A fresh partial block parks.
    var tail: [1000]u8 = @splat(0x31);
    _ = try w.wp.write(io, .{ .ino = 30, .offset = 2 * wt.BS, .payload = .{ .bytes = &tail }, .class = .entire_op, .file = World.striped(wt.BS) });
    try testing.expect(w.wp.table.has(30, 2));
    try testing.expect(w.wp.holdsCustody(30));
    w.wp.flushIno(io, 30);
    try testing.expect(!w.wp.holdsCustody(30));
    try testing.expect(w.wp.holdsCustody(31));
    w.wp.flushIno(io, 31);
    try testing.expect(!w.wp.holdsCustody(31));
    try testing.expect(!w.wp.overlay.anyOpenFast());
    try testing.expectEqual(@as(usize, 0), w.wp.epochs.openCount());
}

test "the write-back timer's drain (decision 0003 §AJ): `drainAged` flushes exactly the parked blocks born by the cutoff and keeps the younger ones (reporting the oldest kept birth); a later cutoff lands the rest; an overlay record and its epoch drain whole whatever their age; an ino holding nothing lands nothing" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, true);
    defer w.deinit(testing.allocator);
    try testing.expectEqual(WritePath.AgedDrain{ .landed = false, .kept_born_ns = null }, w.wp.drainAged(io, 40, std.math.maxInt(u64)));
    // Two fresh partial blocks of a live file, born apart.
    _ = try w.f.publishBlock(40, 0, 0xAA);
    var tail: [1000]u8 = @splat(0x41);
    _ = try w.wp.write(io, .{ .ino = 40, .offset = 2 * wt.BS, .payload = .{ .bytes = &tail }, .class = .entire_op, .file = World.striped(wt.BS) });
    const born2 = w.wp.table.get(40, 2).?.born_ns;
    while (clock.monotonicNs() == born2) {}
    _ = try w.wp.write(io, .{ .ino = 40, .offset = 5 * wt.BS, .payload = .{ .bytes = &tail }, .class = .entire_op, .file = World.striped(3 * wt.BS) });
    const born5 = w.wp.table.get(40, 5).?.born_ns;
    try testing.expect(born5 > born2);
    // The cutoff between them: block 2 lands, block 5 stays.
    try testing.expectEqual(WritePath.AgedDrain{ .landed = true, .kept_born_ns = born5 }, w.wp.drainAged(io, 40, born2));
    try testing.expect(!w.wp.table.has(40, 2));
    try testing.expect(w.wp.table.has(40, 5));
    // Nothing aged by a cutoff before both: nothing lands, block 5 kept.
    try testing.expectEqual(WritePath.AgedDrain{ .landed = false, .kept_born_ns = born5 }, w.wp.drainAged(io, 40, born5 - 1));
    try testing.expectEqual(WritePath.AgedDrain{ .landed = true, .kept_born_ns = null }, w.wp.drainAged(io, 40, born5));
    try testing.expect(!w.wp.holdsCustody(40));
    // An overwrite half-block: an open overlay record — drained whole at
    // any cutoff (its bytes are already on the device), the epoch closed.
    _ = try w.f.publishBlock(41, 0, 0xBB);
    var half: [wt.BS / 2]u8 align(4096) = @splat(0x42);
    _ = try w.wp.write(io, .{ .ino = 41, .offset = 0, .payload = .{ .bytes = &half }, .class = .shared, .file = World.striped(wt.BS) });
    try testing.expect(w.wp.holdsCustody(41));
    try testing.expectEqual(WritePath.AgedDrain{ .landed = true, .kept_born_ns = null }, w.wp.drainAged(io, 41, 0));
    try testing.expect(!w.wp.holdsCustody(41));
    try testing.expect(!w.wp.overlay.anyOpenFast());
    try testing.expectEqual(@as(usize, 0), w.wp.epochs.openCount());
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

test "the save window: while an epoch's ONE save is in flight the block stays bound — a racing W1 patch declines (it would land in the key the save displaces), the overlay declines, the park bases on B; the save displaces only what it parked; B, the file's alone once saved, takes the parked pages in place (§AJ.7)" {
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
    try testing.expect(final.eql(b));
    try testing.expectEqual(@as(usize, 1), w.f.frees.freed.items.len);
    try testing.expect(w.f.frees.freed.items[0].eql(a1));
    try testing.expectEqual(drift0, counters.tripwireCount("rewrite_shadow_displaced_drift"));
    try testing.expectEqual(untracked0, counters.tripwireCount("block_untracked_free_refusals"));
    try testing.expectEqual(@as(u64, 2), w.f.alloc_a.usedBlocks() + w.f.alloc_b.usedBlocks());
}

test "the one-authority screen never parks under (3): a write meeting an overlay record whose settle keeps failing drops the block guard for the settle's ladder (a sibling takes the guard mid-ladder) and restarts its ladder once the record landed — the write acks, the record fed to the epoch whose save publishes it" {
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
    const feeds0 = device_overlay.stats.overlay_epoch_feeds.load();
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
    // The record landed as a FEED (a fresh block rides the epoch); the
    // epoch shadows the block until its save.
    try testing.expectEqual(feeds0 + 1, device_overlay.stats.overlay_epoch_feeds.load());
    try testing.expectEqual(@as(u32, 0), w.wp.overlay.registry.open_count.load(.monotonic));
    try testing.expect(w.wp.epochs.bindsBlock(70, 0));
    try testing.expect(w.wp.epochs.close(io, 70, w.wp.closeSeams()) == .swapped);
    const b = w.f.map.seam().lookup(70, 0) orelse return error.TestUnexpectedResult;
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(b.vol_tag, b.offset, &got);
    try testing.expectEqualSlices(u8, &half, got[0 .. wt.BS / 2]);
}

test "the ordered law's floor under a FRESH stream riding the epoch (decision 0003 §AL.5): the floor is the lowest block the epoch shadows — open or saving — not 0 for the epoch's whole life: whole blocks 2 and 3 fed ⇒ floor = block 2's start; the save lands them ⇒ no custody, the floor lifts; blocks 5 and 6 fed ⇒ block 5's start; a parked partial block 4 beside them ⇒ its lowest covered byte" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, true);
    defer w.deinit(testing.allocator);
    try w.f.map.create(75, .striped, 0);
    var whole: [wt.BS]u8 align(4096) = undefined;
    fill(&whole, 3);
    try testing.expectEqual(@as(u64, std.math.maxInt(u64)), w.wp.custodyFloor(io, 75));
    // Blocks 2 and 3, not 0 and 1: the detached drains feed in either
    // order, and two blocks shadowed never reach block 2's floor of three
    // (the coverage hint would close the epoch under the assertions).
    for ([_]u32{ 2, 3 }) |b| {
        const o = try w.wp.write(io, .{ .ino = 75, .offset = @as(u64, b) * wt.BS, .payload = .{ .bytes = &whole }, .class = .entire_op, .file = World.striped(@as(u64, b) * wt.BS) });
        try testing.expectEqual(@as(u32, 1), o.count(.overlay));
    }
    w.wp.quiesce(io); // the drains fed both
    try testing.expect(w.wp.epochs.bindsBlock(75, 2) and w.wp.epochs.bindsBlock(75, 3));
    try testing.expect(w.f.map.seam().lookup(75, 2) == null);
    try testing.expectEqual(@as(u64, 2 * wt.BS), w.wp.custodyFloor(io, 75));
    // The save names them: no custody left, the claim may journal past them.
    try testing.expect(w.wp.epochs.close(io, 75, w.wp.closeSeams()) == .swapped);
    try testing.expectEqual(@as(u64, std.math.maxInt(u64)), w.wp.custodyFloor(io, 75));
    for ([_]u32{ 5, 6 }) |b| {
        _ = try w.wp.write(io, .{ .ino = 75, .offset = @as(u64, b) * wt.BS, .payload = .{ .bytes = &whole }, .class = .entire_op, .file = World.striped(@as(u64, b) * wt.BS) });
    }
    w.wp.quiesce(io);
    try testing.expectEqual(@as(u64, 5 * wt.BS), w.wp.custodyFloor(io, 75));
    // A parked partial block below the epoch's lowest: its covered byte wins.
    var tail: [PAGE]u8 align(4096) = @splat(0x77);
    const o = try w.wp.write(io, .{ .ino = 75, .offset = 4 * wt.BS + 2 * PAGE, .payload = .{ .bytes = &tail }, .class = .entire_op, .file = World.striped(4 * wt.BS) });
    try testing.expectEqual(@as(u32, 1), o.count(.extent) + o.count(.accumulate_park));
    try testing.expectEqual(@as(u64, 4 * wt.BS + 2 * PAGE), w.wp.custodyFloor(io, 75));
    w.wp.flushIno(io, 75);
    try testing.expectEqual(@as(u64, std.math.maxInt(u64)), w.wp.custodyFloor(io, 75));
    for ([_]u32{ 2, 3, 4, 5, 6 }) |b| try testing.expect(w.f.map.seam().lookup(75, b) != null);
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

// The write-back storm at a parked cap of 8 (`rung-10-writeback-timer.md`):
// W2 page overlays held every table entry without a pool cell, so a full
// checkout holding a fresh cell found no entry (`active_block_table_full`,
// must-stay-0) and parked the writer at a gate whose pool had room.
test "the active-block table holds the parked cap's buffers AND the extents beside them: with every extent slot taken, a full checkout still finds its entry (no tripwire, no gate visit) and a small piece past the extent cap parks a full buffer" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .sync_inline, false);
    defer w.deinit(testing.allocator);
    const cap = w.wp.cfg.parked_cap;
    const tripped0 = counters.tripwireCount("active_block_table_full");
    const waits0 = stats.parked_gate_waits.load();
    // `cap` first pieces into fresh hole blocks: one W2 extent each.
    var page: [PAGE]u8 = undefined;
    fill(&page, 0x61);
    var ino: u64 = 100;
    while (ino < 100 + cap) : (ino += 1) {
        try w.f.map.create(ino, .striped, 0);
        const o = try w.wp.write(io, .{ .ino = ino, .offset = 0, .payload = .{ .bytes = &page }, .class = .entire_op, .file = World.striped(0) });
        try testing.expectEqual(@as(u32, 1), o.count(.extent));
    }
    try testing.expectEqual(@as(u32, @intCast(cap)), w.wp.table.parkedCount());
    // A half-block piece takes a cell from the empty pool and its entry.
    try w.f.map.create(200, .striped, 0);
    var half: [wt.BS / 2 + 100]u8 = undefined;
    fill(&half, 0x62);
    const o_full = try w.wp.write(io, .{ .ino = 200, .offset = 0, .payload = .{ .bytes = &half }, .class = .entire_op, .file = World.striped(0) });
    try testing.expectEqual(@as(u32, 1), o_full.count(.accumulate_park));
    // A small first piece past the extent cap parks a full buffer.
    try w.f.map.create(201, .striped, 0);
    const o_small = try w.wp.write(io, .{ .ino = 201, .offset = 0, .payload = .{ .bytes = &page }, .class = .entire_op, .file = World.striped(0) });
    try testing.expectEqual(@as(u32, 1), o_small.count(.accumulate_park));
    try testing.expectEqual(@as(u32, @intCast(cap + 2)), w.wp.table.parkedCount());
    try testing.expectEqual(tripped0, counters.tripwireCount("active_block_table_full"));
    try testing.expectEqual(waits0, stats.parked_gate_waits.load());
    // Every parked byte lands at its flush.
    ino = 100;
    while (ino < 100 + cap) : (ino += 1) w.wp.flushIno(io, ino);
    w.wp.flushIno(io, 200);
    w.wp.flushIno(io, 201);
    w.wp.quiesce(io);
    try testing.expectEqual(@as(u32, 0), w.wp.table.parkedCount());
    var got: [wt.BS]u8 = undefined;
    const b = w.f.map.seam().lookup(201, 0) orelse return error.BlockUnmapped;
    w.f.dev.peek(b.vol_tag, b.offset, &got);
    try testing.expectEqualSlices(u8, &page, got[0..PAGE]);
    try testing.expect(std.mem.allEqual(u8, got[PAGE..], 0));
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

// rung-10-seqwrite: 1M seq write ×4 on RAM-backed NVMe kept four writes of
// each 4 MiB block in flight, and the overlay arm held the block guard
// across its DMA — 54 % of the writes parked 7.2 ms on (3) behind a
// sibling's DMA. The range claims exclude overlapping DMAs (§3.3); the guard
// covers the claim, not the device.
test "two writes into disjoint halves of one block are in flight at once through the overlay: the second reaches its DMA while the first is held in its own — the block guard is never held across a store's DMA" {
    const io = testing.io;
    const w = try World.init(testing.allocator, .adaptive, true);
    defer w.deinit(testing.allocator);
    var held: HeldDma = .{};
    w.f.dev.hook = held.hook();
    try w.f.map.create(5, .striped, 0);
    const Writer = struct {
        w: *World,
        offset: u64,
        buf: [wt.BS / 2]u8 align(4096) = undefined,
        result: anyerror!Outcome = error.NotRun,
        fn run(self: *@This()) void {
            self.result = self.w.wp.write(testing.io, .{ .ino = 5, .offset = self.offset, .payload = .{ .bytes = &self.buf }, .class = .entire_op, .file = World.striped(self.offset) });
        }
    };
    var a: Writer = .{ .w = w, .offset = 0 };
    var b: Writer = .{ .w = w, .offset = wt.BS / 2 };
    fill(&a.buf, 0x51);
    fill(&b.buf, 0x52);
    const ta = try std.Thread.spawn(.{}, Writer.run, .{&a});
    while (held.hits.load(.acquire) == 0) std.Thread.yield() catch {};
    const stripe = &w.wp.locks.locks[w.wp.locks.stripeOf(5, 0)];
    const tb = try std.Thread.spawn(.{}, Writer.run, .{&b});
    // B either reaches its own DMA or parks on the block guard A holds.
    while (held.hits.load(.acquire) < 2 and stripe.waiters(io) == 0) std.Thread.yield() catch {};
    const both_in_flight = held.hits.load(.acquire) == 2;
    held.release();
    ta.join();
    tb.join();
    try testing.expect(both_in_flight);
    try testing.expectEqual(@as(u32, 1), (try a.result).count(.overlay));
    try testing.expectEqual(@as(u32, 1), (try b.result).count(.overlay));
    // The second CQE completed the coverage: the drain publishes both halves.
    w.wp.quiesce(io);
    const bound = w.f.map.seam().lookup(5, 0) orelse return error.BlockUnmapped;
    var got: [wt.BS]u8 = undefined;
    w.f.dev.peek(bound.vol_tag, bound.offset, &got);
    try testing.expectEqualSlices(u8, &a.buf, got[0 .. wt.BS / 2]);
    try testing.expectEqualSlices(u8, &b.buf, got[wt.BS / 2 ..]);
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
