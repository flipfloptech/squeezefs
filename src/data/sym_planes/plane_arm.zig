//! The data planes' DEVICE-PATH arms over a mounted `DataPlane` (Linux;
//! charter §5.5 "Data-plane planes under the symmetric model", §6.2 "the
//! grants feed the allocator"; `docs/porting/sym_data_planes.md` §3–§4):
//! what the N-daemon posture binds per DATA volume once the cluster wire is
//! up.
//!
//! The HOLDER (the manager — the allocation lease's first-come holder at
//! term 1, `alloc_lease.zig`'s record on volume 0): a `DataBitmap` over its
//! own `BlockAllocator`'s bits (the bitmap IS the free list from here on:
//! a carve SETS bits at the lowest clear run, a shipped terminal free
//! CLEARS them), the grant `Ledger`, the served `BlockGrant` /
//! `ReturnBlocks` / `FreeBlocks` behind ONE dispatcher by `vol_tag`, and
//! the holder's OWN window — its mints come from its own carves too, so
//! its cursor never walks into a range it granted. The bitmap's pages and
//! deltas have no device home in this wave (`journal = null`): a holder's
//! death loses the RAM carve, and the successor re-learns the granted
//! ranges from every live writer's declared windows (charter §5.5) and the
//! published refs — the leak class fsck C2/C6 reports, never a double
//! allocation (a joiner mints only inside a range the holder set).
//!
//! The WRITER (a joiner): a `GrantWindow` per data volume as the
//! allocator's ONE mint source, topped up as a PULL over the wire — inline
//! at the mint when the window is empty or below the 50 % mark (the
//! `topup_inflight` latch keeps it single-flight; a holder RTT on the
//! write path once per window, never per block); terminal frees SHIP to
//! the holder (`free_ship.Shipper` as the reclaimer's `FreeTarget`,
//! flushed at the cadence), the verdicts purging the tiers and retiring
//! this mount's own word.

const std = @import("std");
const builtin = @import("builtin");
const Allocator = std.mem.Allocator;
const Io = std.Io;

const counters = @import("../../core/counters.zig");
const log = @import("../../core/log.zig");
const service = @import("../../wire/service.zig");
const verbs = @import("../../wire/verbs.zig");
const block_alloc = @import("../block_alloc.zig");
const block_refs = @import("../block_refs.zig");
const reclaim = @import("../reclaim.zig");
const router = @import("../router.zig");
const plane_mod = @import("../plane.zig");
const data_bitmap = @import("data_bitmap.zig");
const block_grant = @import("block_grant.zig");
const free_ship = @import("free_ship.zig");
const wire_bind = @import("wire_bind.zig");
const records = @import("records.zig");
const token_cache = @import("../../meta/sym/tokens/token_cache.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("data/sym_planes/plane_arm is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const DataPlane = plane_mod.DataPlane;
pub const Link = token_cache.Link;

pub const stats = struct {
    /// Data volumes this process holds the grant plane of (the holder).
    pub var alloc_lease_held_volumes: counters.Gauge = .{};
    /// Data volumes this process writes through a grant window (a joiner).
    pub var block_grant_writer_volumes: counters.Gauge = .{};
    /// Inline top-ups the mint path pulled (the empty-window arm).
    pub var block_grant_inline_topups: counters.ShardedCounter = .{};
    /// Shipped-free batches flushed at the cadence.
    pub var free_ship_cadence_flushes: counters.ShardedCounter = .{};
    /// Unconsumed window runs a joiner returned to the holder at its stop.
    pub var block_grant_window_runs_returned: counters.ShardedCounter = .{};
};

/// The grant ledger's writer key of a wire identity: `(node_token,
/// mount_slot)` folded to the ledger's u32 (a KEY, never an allocation
/// authority; the holder itself is writer 0 = appender 0).
pub fn writerKeyOf(id: records.WriterIdentity) u32 {
    var h = std.hash.XxHash3.init(0);
    h.update(std.mem.asBytes(&id.node_token));
    h.update(std.mem.asBytes(&id.mount_slot));
    return @truncate(h.final());
}

/// The holder's own key.
pub const HOLDER_WRITER: u32 = 0;

/// The holder's appender directory: a grant is served to a LIVE member of
/// the home shard only (an unknown identity is `Rejected`).
pub const WriterDirectory = struct {
    ctx: *anyopaque,
    isMember: *const fn (*anyopaque, records.WriterIdentity) bool,
};

/// Open grants the ledger tracks per volume: the appender capacity's order
/// (`GRANTS_MAX` windows per writer × the joiners a venue reaches).
pub const LEDGER_CAPACITY: usize = 1024;

// ---------------------------------------------------------------------------
// The inline-top-up grant source over a writer arm.
// ---------------------------------------------------------------------------

/// The allocator's `GrantSource` over a `WriterArm`: the arm's mint (the
/// PULL folded in at the 50 % mark, single-flight — the charter's "writer
/// tops up at 50 % as a PULL"; an empty window answers its class), the
/// inline asks counted, and a landed top-up ringing the allocator's supply
/// edge: a flush parked there for visible supply (`enospc.
/// allocateBlockGraceBounded`) wakes on the grant, not on its slice.
const ArmSource = struct {
    arm: *block_grant.WriterArm,
    alloc: *block_alloc.BlockAllocator,
    io: Io,

    fn mint(ctx: *anyopaque) block_alloc.MintAnswer {
        const self: *ArmSource = @ptrCast(@alignCast(ctx));
        const m = self.arm.mintBlock();
        if (m.asked) {
            stats.block_grant_inline_topups.inc();
            self.alloc.supply_edge.notifyWaiters(self.io);
        }
        return m.answer;
    }

    fn giveBack(ctx: *anyopaque, idx: u64) bool {
        const self: *ArmSource = @ptrCast(@alignCast(ctx));
        return self.arm.window.giveBack(idx);
    }

    fn remaining(ctx: *anyopaque) u64 {
        const self: *ArmSource = @ptrCast(@alignCast(ctx));
        return self.arm.window.remaining();
    }

    fn wantsTopup(ctx: *anyopaque) bool {
        const self: *ArmSource = @ptrCast(@alignCast(ctx));
        return self.arm.window.wantsTopup();
    }

    fn noteExhausted(ctx: *anyopaque) void {
        const self: *ArmSource = @ptrCast(@alignCast(ctx));
        self.arm.window.noteExhausted();
    }

    const vtable: block_alloc.GrantSource.VTable = .{ .mint = &mint, .giveBack = &giveBack, .remaining = &remaining, .wantsTopup = &wantsTopup, .noteExhausted = &noteExhausted };

    fn source(self: *ArmSource) block_alloc.GrantSource {
        return .{ .ctx = self, .vtable = &vtable };
    }
};

// ---------------------------------------------------------------------------
// The holder (the manager).
// ---------------------------------------------------------------------------

pub const HolderPlane = struct {
    gpa: Allocator,
    io: Io,
    plane: *DataPlane,
    dir: WriterDirectory,
    /// `router.MAX_VOLUMES` rows, `n_vols` armed (module law above).
    vols: []Vol,
    n_vols: usize,

    const Vol = struct {
        vol_tag: u64,
        ordinal: u16,
        /// The allocation-lease term this holder acquired for the volume
        /// (every bitmap delta carries it; the release names it).
        term: u64,
        alloc: *block_alloc.BlockAllocator,
        bitmap: *data_bitmap.DataBitmap,
        grants: block_grant.HolderGrants,
        frees: free_ship.FreeExecutor,
        handlers: wire_bind.HolderHandlers,
        /// The holder's own window over its own carves.
        own_sink: block_grant.LocalSink,
        own: block_grant.WriterArm,
        own_src: ArmSource,
        holder: *HolderPlane,
    };

    /// Arm every data volume: the bitmap over the allocator at the
    /// volume's lease term (`terms[ordinal]`), the ledger, the executor,
    /// the holder's own window installed as the allocator's mint source.
    /// The table is pre-sized at the router's bound: every `Vol` is pointed
    /// to by the allocator's seams (lease, mint source, sinks), so a row
    /// never moves — the online `volume add-data` arms the NEXT row in
    /// place (`armVolume`).
    pub fn create(gpa: Allocator, io: Io, plane: *DataPlane, dir: WriterDirectory, terms: []const u64, writers_known: u64) Allocator.Error!*HolderPlane {
        const self = try gpa.create(HolderPlane);
        errdefer gpa.destroy(self);
        const n = plane.vols.n();
        std.debug.assert(terms.len == n);
        const vols = try gpa.alloc(Vol, router.MAX_VOLUMES);
        errdefer gpa.free(vols);
        self.* = .{ .gpa = gpa, .io = io, .plane = plane, .dir = dir, .vols = vols, .n_vols = 0 };
        errdefer for (vols[0..self.n_vols]) |*v| self.disarm(v);
        for (terms, 0..) |term, i| try self.armVolume(@intCast(i), term, writers_known);
        log.info("data planes: the grant plane armed as the HOLDER of {d} data volume(s) (the bitmap over the allocator; grants carve at the frontier; the own window is the mint source)", .{n});
        return self;
    }

    /// Arm ONE volume (the plane's row at `ordinal`, already open): at
    /// create for every volume, online for a volume `volume add-data` just
    /// attached — the holder's bitmap, ledger, free executor and own window
    /// over its allocator. The lease term is the caller's (acquired on tree
    /// 0 before this arm).
    pub fn armVolume(self: *HolderPlane, ordinal: u16, term: u64, writers_known: u64) Allocator.Error!void {
        const gpa = self.gpa;
        const plane = self.plane;
        std.debug.assert(ordinal == self.n_vols and ordinal < router.MAX_VOLUMES);
        const r = plane.vols.rows[ordinal].?;
        const v = &self.vols[ordinal];
        v.* = .{
            .vol_tag = r.vol_tag,
            .ordinal = ordinal,
            .term = term,
            .alloc = r.alloc,
            .bitmap = undefined,
            .grants = undefined,
            .frees = undefined,
            .handlers = undefined,
            .own_sink = undefined,
            .own = .{ .capacity_blocks = r.alloc.capacityBlocks() },
            .own_src = undefined,
            .holder = self,
        };
        v.bitmap = try data_bitmap.DataBitmap.init(gpa, r.alloc, .{
            .vol_tag = r.vol_tag,
            .term = term,
            .blocks = r.alloc.capacityBlocks(),
            .ring_capacity = data_bitmap.ringCapacity(block_grant.BLOCK_GRANT_FLOOR, @max(writers_known, 1), free_ship.FREE_BATCH_MAX),
        });
        errdefer v.bitmap.deinit();
        v.grants = .{ .ledger = try block_grant.Ledger.init(gpa, LEDGER_CAPACITY), .bitmap = v.bitmap };
        v.frees = .{ .alloc = r.alloc, .durable = .{ .ctx = v, .refcount = &durableRefcount }, .ladder = .{ .ctx = v, .free = &ladderFree } };
        v.handlers = .{ .vol_tag = r.vol_tag, .grants = &v.grants, .frees = &v.frees, .writer_ctx = self, .writerId = &writerIdFn };
        // The own window is bound to its sink: every top-up retires the
        // grants it consumed (the ledger holds OPEN grants only).
        v.own_sink = .{ .holder = &v.grants, .writer = HOLDER_WRITER, .window = &v.own.window };
        v.own.sink = v.own_sink.sink();
        v.own_src = .{ .arm = &v.own, .alloc = r.alloc, .io = self.io };
        // The holder's bits and frees: the bitmap's finish-free sink
        // clears a carved bit; the shipped-free executor adjudicates
        // the untracked class; the mint source is the own window.
        r.alloc.finish_free_sink = v.bitmap.finishFreeSink();
        r.alloc.untracked_gate = v.frees.untrackedGate();
        r.alloc.grant_source = v.own_src.source();
        r.alloc.lease = .{ .ctx = v, .vtable = &holder_lease_vtable };
        plane.reclaimer.setFreeTarget(v.ordinal, v.frees.localTarget());
        self.n_vols += 1;
        stats.alloc_lease_held_volumes.set(@intCast(self.n_vols));
    }

    fn disarm(self: *HolderPlane, v: *Vol) void {
        self.plane.reclaimer.setFreeTarget(v.ordinal, null);
        v.alloc.grant_source = null;
        v.alloc.lease = null;
        v.alloc.finish_free_sink = null;
        v.alloc.untracked_gate = null;
        v.grants.ledger.deinit();
        v.bitmap.deinit();
    }

    /// `df` per the rule on the holder: the bitmap's population and
    /// frontier, its own unconsumed window.
    fn holderViewFn(ctx: *anyopaque) ?block_alloc.HolderView {
        const v: *Vol = @ptrCast(@alignCast(ctx));
        const word = v.grants.word();
        return .{ .bitmap_blocks = word.blocks, .population = word.population, .frontier = word.frontier, .window_remaining = v.own.window.remaining() };
    }

    /// The mount seed landed before the arm (the plane opened first); the
    /// bitmap re-synced its population from the allocator's bits at init.
    fn noteMountSeedFn(_: *anyopaque, _: u64, _: u64, _: []const u64) void {}

    const holder_lease_vtable: block_alloc.AllocationLease.VTable = .{ .holderView = &holderViewFn, .noteMountSeed = &noteMountSeedFn };

    pub fn destroy(self: *HolderPlane) void {
        for (self.vols[0..self.n_vols]) |*v| self.disarm(v);
        stats.alloc_lease_held_volumes.set(0);
        self.gpa.free(self.vols);
        self.gpa.destroy(self);
    }

    /// Volumes armed (the release walks them).
    pub fn armed(self: *const HolderPlane) usize {
        return self.n_vols;
    }

    /// The per-volume words (`vol_tag`, lease term) for the release.
    pub const Held = struct { vol_tag: u64, term: u64 };

    pub fn held(self: *const HolderPlane, ordinal: usize) Held {
        return .{ .vol_tag = self.vols[ordinal].vol_tag, .term = self.vols[ordinal].term };
    }

    fn volOf(self: *HolderPlane, vol_tag: u64) ?*Vol {
        for (self.vols[0..self.n_vols]) |*v| if (v.vol_tag == vol_tag) return v;
        return null;
    }

    // ONE handler per verb over every volume (the wire plane registers
    // them before its listener): dispatch by the request's `vol_tag`.

    pub fn serveGrant(ctx: *anyopaque, req: *const service.Request, scratch: []u8) service.Answer {
        const self: *HolderPlane = @ptrCast(@alignCast(ctx));
        const v = self.volOf(req.payload.block_grant.vol_tag) orelse return service.Answer.refused(error.Busy, "this mount does not hold the lease of that volume");
        return wire_bind.HolderHandlers.serveGrant(&v.handlers, req, scratch);
    }

    pub fn serveReturn(ctx: *anyopaque, req: *const service.Request, scratch: []u8) service.Answer {
        const self: *HolderPlane = @ptrCast(@alignCast(ctx));
        const v = self.volOf(req.payload.return_blocks.vol_tag) orelse return service.Answer.refused(error.Busy, "this mount does not hold the lease of that volume");
        return wire_bind.HolderHandlers.serveReturn(&v.handlers, req, scratch);
    }

    pub fn serveFrees(ctx: *anyopaque, req: *const service.Request, scratch: []u8) service.Answer {
        const self: *HolderPlane = @ptrCast(@alignCast(ctx));
        const v = self.volOf(req.payload.free_blocks.head.vol_tag) orelse return service.Answer.refused(error.Busy, "this mount does not hold the lease of that volume");
        return wire_bind.HolderHandlers.serveFrees(&v.handlers, req, scratch);
    }

    fn writerIdFn(ctx: *anyopaque, id: records.WriterIdentity) ?u32 {
        const self: *HolderPlane = @ptrCast(@alignCast(ctx));
        return if (self.dir.isMember(self.dir.ctx, id)) writerKeyOf(id) else null;
    }

    /// The durable population of `(vol_tag, idx)`: the refs ledger's
    /// range probe through the plane's store (this holder's view).
    fn durableRefcount(ctx: *anyopaque, vol_tag: u64, idx: u64) u32 {
        const v: *Vol = @ptrCast(@alignCast(ctx));
        return block_refs.refcount(v.holder.plane.refs_store, v.holder.gpa, vol_tag, idx) catch 0;
    }

    /// The terminal ladder with the holder's authority (the executor set
    /// the marker): the reclaimer's own free under the block's live stamp.
    fn ladderFree(ctx: *anyopaque, idx: u64) reclaim.FreeOutcome {
        const v: *Vol = @ptrCast(@alignCast(ctx));
        return v.holder.plane.reclaimer.freeBlock(v.holder.io, v.ordinal, idx, v.alloc.stampOf(idx));
    }

    /// `df` per the rule: the population minus the own unconsumed window.
    pub fn writeStatsJson(self: *HolderPlane, w: *Io.Writer) Io.Writer.Error!void {
        try w.writeAll("[");
        for (self.vols[0..self.n_vols], 0..) |*v, i| {
            if (i != 0) try w.writeByte(',');
            const word = v.grants.word();
            try w.print("{{\"vol_tag\":\"{x:0>16}\",\"population\":{d},\"frontier\":{d},\"open_grants\":{d},\"writers\":{d},\"own_window_remaining\":{d},\"used_blocks\":{d}}}", .{ v.vol_tag, word.population, word.frontier, v.grants.ledger.openCount(), v.grants.writers(), v.own.window.remaining(), word.population -| v.own.window.remaining() });
        }
        try w.writeAll("]");
    }
};

// ---------------------------------------------------------------------------
// The writer (a joiner).
// ---------------------------------------------------------------------------

pub const WriterPlane = struct {
    gpa: Allocator,
    io: Io,
    plane: *DataPlane,
    /// The lease holder's session (the manager's listener).
    link: Link,
    identity: verbs.WireIdentity,
    vols: []Vol,

    const Vol = struct {
        vol_tag: u64,
        ordinal: u16,
        alloc: *block_alloc.BlockAllocator,
        arm: block_grant.WriterArm,
        src: ArmSource,
        shipper: free_ship.Shipper,
        /// `Freed` verdicts acked since the holder's last word: the holder
        /// cleared those bits, so `df` projects them until the next reply
        /// carries a fresh population (reset there).
        freed_since_word: std.atomic.Value(u64) = .init(0),
        writer: *WriterPlane,
    };

    /// Arm every data volume: the window as the mint source (an empty
    /// window pulls its first grant at the first mint), the shipper as
    /// the reclaimer's free target.
    pub fn create(gpa: Allocator, io: Io, plane: *DataPlane, link: Link, identity: verbs.WireIdentity) Allocator.Error!*WriterPlane {
        const self = try gpa.create(WriterPlane);
        errdefer gpa.destroy(self);
        const n = plane.vols.n();
        const vols = try gpa.alloc(Vol, n);
        errdefer gpa.free(vols);
        self.* = .{ .gpa = gpa, .io = io, .plane = plane, .link = link, .identity = identity, .vols = vols };
        for (plane.vols.rows[0..n], 0..) |row, i| {
            const r = row.?;
            const v = &vols[i];
            v.* = .{
                .vol_tag = r.vol_tag,
                .ordinal = @intCast(r.ordinal),
                .alloc = r.alloc,
                .arm = .{ .capacity_blocks = r.alloc.capacityBlocks() },
                .src = undefined,
                .shipper = .{ .vol_tag = r.vol_tag, .tracking = .{ .ctx = v, .purge = &trackPurge, .retire = &trackRetire, .release = &trackRelease }, .lease_epoch = .{ .ctx = v, .epoch = &leaseEpoch } },
                .writer = self,
            };
            // The wire ask parks on its round trip: a lost single-flight
            // latch at an empty window is the retryable class, never a wait.
            v.arm.sink = .{ .ctx = v, .ask = &askGrant, .inline_answer = false };
            v.src = .{ .arm = &v.arm, .alloc = r.alloc, .io = io };
            v.shipper.client = .{ .ctx = v, .send = &sendFrees, .resolve = &resolveHolder };
            r.alloc.grant_source = v.src.source();
            r.alloc.lease = .{ .ctx = v, .vtable = &writer_lease_vtable };
            plane.reclaimer.setFreeTarget(v.ordinal, v.shipper.freeTarget());
            // The first PULL at the arm: an empty window is below the 50 %
            // mark, and the reply's holder word is what `df` reads before
            // this mount writes a byte. A holder not yet serving answers
            // `Busy` → the first mint re-asks.
            _ = v.arm.topup();
        }
        stats.block_grant_writer_volumes.set(@intCast(n));
        log.info("data planes: {d} data volume(s) armed as a NON-holder — every mint from a ranged grant pulled from the lease holder, every terminal free shipped to it", .{n});
        return self;
    }

    pub fn destroy(self: *WriterPlane) void {
        for (self.vols) |*v| {
            // The pending frees ship; every unconsumed run of the window
            // RETURNS to the holder (its bits clear again) — a leave never
            // leaves a granted run SET at the holder.
            _ = v.shipper.flush();
            self.returnWindow(v);
            self.plane.reclaimer.setFreeTarget(v.ordinal, null);
            v.alloc.grant_source = null;
            v.alloc.lease = null;
        }
        stats.block_grant_writer_volumes.set(0);
        self.gpa.free(self.vols);
        self.gpa.destroy(self);
    }

    /// `ReturnBlocks` per unconsumed run (the window drained first, so no
    /// mint races the return).
    fn returnWindow(self: *WriterPlane, v: *Vol) void {
        var runs: block_grant.Grants = .{};
        v.arm.window.drain(&runs);
        for (runs.slice()) |g| {
            _ = self.link.call(self.link.ctx, .{ .return_blocks = .{ .volume = 0, .reserved = 0, .len = g.len, .writer = self.identity, .vol_tag = v.vol_tag, .start = g.start } }) catch |e| {
                log.warn("data planes: the window run [{d}, +{d}) of vol {x} could not be returned at the stop ({s}) — the holder's next derivation returns it", .{ g.start, g.len, v.vol_tag, @errorName(e) });
                continue;
            };
            stats.block_grant_window_runs_returned.inc();
        }
    }

    /// `df` per the rule on a joiner: the holder's last word with this
    /// mount's own window; null before the first reply (the window alone
    /// is the supply then).
    fn holderViewFn(ctx: *anyopaque) ?block_alloc.HolderView {
        const v: *Vol = @ptrCast(@alignCast(ctx));
        const w = v.arm.last_word orelse return null;
        return .{ .bitmap_blocks = w.blocks, .population = w.population -| v.freed_since_word.load(.acquire), .frontier = w.frontier, .window_remaining = v.arm.window.remaining() };
    }

    fn noteMountSeedFn(_: *anyopaque, _: u64, _: u64, _: []const u64) void {}

    const writer_lease_vtable: block_alloc.AllocationLease.VTable = .{ .holderView = &holderViewFn, .noteMountSeed = &noteMountSeedFn };

    /// The cadence: every volume's pending shipped frees flushed.
    pub fn tick(self: *WriterPlane) void {
        for (self.vols) |*v| {
            if (v.shipper.flush() == .served) stats.free_ship_cadence_flushes.inc();
        }
    }

    /// `BlockGrant` over the wire: the PULL.
    fn askGrant(ctx: *anyopaque, want: u64, held: u64) block_grant.GrantAnswer {
        const v: *Vol = @ptrCast(@alignCast(ctx));
        const rep = v.writer.link.call(v.writer.link.ctx, .{ .block_grant = .{ .volume = 0, .reserved = 0, .want = @intCast(@min(want, std.math.maxInt(u32))), .writer = v.writer.identity, .vol_tag = v.vol_tag, .held_unconsumed = held } }) catch |e| return switch (e) {
            error.Deferred, error.MembershipPending => .deferred,
            error.Rejected => .refused,
            else => .holder_unreachable,
        };
        const g = rep.block_grant;
        // The SCREEN of the holder's word: a malformed head, a word naming
        // another geometry, a run outside the volume or empty — the whole
        // reply is refused (the wire integer is never an authority).
        if (!g.head.check() or g.head.blocks != v.alloc.capacityBlocks()) return .refused;
        const word: block_grant.HolderWord = .{ .blocks = g.head.blocks, .population = g.head.population, .frontier = g.head.frontier };
        // A fresh word supersedes the local projection of the acked frees.
        v.freed_since_word.store(0, .release);
        if (g.head.kind == @backingInt(verbs.BlocksGrantedKind.full)) return .{ .full = word };
        var grants: block_grant.Grants = .{};
        for (g.items) |run| {
            if (run.len == 0 or run.start + run.len > v.alloc.capacityBlocks()) return .refused;
            _ = grants.push(.{ .start = run.start, .len = run.len });
        }
        return .{ .granted = .{ .grants = grants, .already = g.head.already == 1, .word = word } };
    }

    fn sendFrees(ctx: *anyopaque, req: free_ship.FreeBlocksRequest) free_ship.ShipError!free_ship.Verdicts {
        const v: *Vol = @ptrCast(@alignCast(ctx));
        var blocks: [free_ship.FREE_BATCH_MAX]u64 = undefined;
        @memcpy(blocks[0..req.blocks.len], req.blocks);
        const rep = v.writer.link.call(v.writer.link.ctx, .{ .free_blocks = .{ .head = .{ .volume = 0, .n = @intCast(req.blocks.len), .reserved = 0, .vol_tag = req.vol_tag, .lease_epoch = req.lease_epoch }, .items = blocks[0..req.blocks.len] } }) catch return error.Transport;
        var out: free_ship.Verdicts = .{};
        for (rep.free_blocks.verdicts) |b| {
            if (out.n == free_ship.FREE_BATCH_MAX) break;
            out.buf[out.n] = switch (b) {
                @backingInt(verbs.FreeVerdict.freed) => .freed,
                @backingInt(verbs.FreeVerdict.non_terminal) => .nonterminal,
                else => .refused,
            };
            out.n += 1;
        }
        return out;
    }

    /// The holder is the manager at its current listener: the shared link
    /// re-dials on its own.
    fn resolveHolder(_: *anyopaque) bool {
        return false;
    }

    /// The lease epoch this wave knows: term 1 (first-come) — a moved
    /// lease is the successor wave's.
    fn leaseEpoch(_: *anyopaque, _: u64) u64 {
        return 1;
    }

    /// The tiers' ONE purge helper through the reclaimer's seam (the step
    /// the local ladder runs at its row 6).
    fn trackPurge(ctx: *anyopaque, _: u64, idx: u64) void {
        const v: *Vol = @ptrCast(@alignCast(ctx));
        if (v.writer.plane.reclaimer.tier_purge) |p| p.purge(p.ctx, v.ordinal, idx);
    }

    /// `Freed` at the holder: this mount's own word retires (the ladder it
    /// never ran locally; on a grant-armed allocator a free-listed block
    /// is never re-minted — the holder's bit is the free list).
    fn trackRetire(ctx: *anyopaque, _: u64, idx: u64) void {
        const v: *Vol = @ptrCast(@alignCast(ctx));
        _ = v.freed_since_word.fetchAdd(1, .acq_rel);
        const verdict = v.alloc.beginFree(idx) catch return;
        if (verdict == .terminal) v.alloc.finishFree(idx);
    }

    /// `NonTerminal` at the holder: this reference only.
    fn trackRelease(ctx: *anyopaque, _: u64, idx: u64) void {
        const v: *Vol = @ptrCast(@alignCast(ctx));
        _ = v.alloc.beginFree(idx) catch return;
    }

    pub fn writeStatsJson(self: *WriterPlane, w: *Io.Writer) Io.Writer.Error!void {
        try w.writeAll("[");
        for (self.vols, 0..) |*v, i| {
            if (i != 0) try w.writeByte(',');
            try w.print("{{\"vol_tag\":\"{x:0>16}\",\"window_remaining\":{d},\"installed\":{d},\"consumed\":{d},\"pending_frees\":{d}}}", .{ v.vol_tag, v.arm.window.remaining(), v.arm.window.installed.load(.monotonic), v.arm.window.consumed.load(.monotonic), v.shipper.pending_n });
        }
        try w.writeAll("]");
    }
};

// ---------------------------------------------------------------------------
// Tests: the pure parts.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "the writer key folds the identity's stable pair: equal for one mount, distinct across mount slots, never the holder's own key for a joiner" {
    const a = writerKeyOf(.{ .node_token = 0xA0, .mount_slot = 0x8000_0001, .writer_id = 1 });
    const b = writerKeyOf(.{ .node_token = 0xA0, .mount_slot = 0x8000_0002, .writer_id = 2 });
    try testing.expectEqual(a, writerKeyOf(.{ .node_token = 0xA0, .mount_slot = 0x8000_0001, .writer_id = 9 }));
    try testing.expect(a != b);
    try testing.expect(a != HOLDER_WRITER and b != HOLDER_WRITER);
}
