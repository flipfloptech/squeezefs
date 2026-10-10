//! The defrag JOB BODIES (spec §1.4 rows `defrag_data / defrag_pack /
//! defrag_meta / defrag_fold`): each one pass = regenerate the plan from
//! durable state, execute it through the EXISTING machinery, terminate on
//! the first no-progress pass or at `DEFRAG_MAX_PASSES` (the termination
//! belt under churn — a maintenance job completes best-effort, never
//! wedges). D2/D1 ride THE ONE MOVER (`backend_ascending` with ONE floor
//! per file; `compact_low`); `--pack` rides the compactor behind the
//! packing lever (a lever-OFF mount refuses the WHOLE arm loud, an
//! adopted lever-ON-era record included); D3 kicks every parked/spilled
//! block through the W2 fold; D4 nudges leaf compactions in chunks of 8,
//! duty-parked between chunks, then ONE lap of the merge sweep. Order-
//! dependent passes stay pipeline width 1. Portable over seams.

const std = @import("std");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const log = @import("../../core/log.zig");
const clock = @import("../../core/clock.zig");
const fabric_mod = @import("../jobs/fabric.zig");
const census = @import("../lifecycle/census.zig");
const mover = @import("../lifecycle/mover.zig");
const metrics = @import("metrics.zig");
const plan_mod = @import("plan.zig");
const pack_compact = @import("pack_compact.zig");

const stats = metrics.stats;
const Fabric = fabric_mod.Fabric;
const JobCtl = fabric_mod.JobCtl;

/// D4's nudge chunk: 8 leaves between duty parks.
pub const META_NUDGE_CHUNK: u32 = 8;

/// What the data arms need of the plane.
pub const DataSeam = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        rows: *const fn (ctx: *anyopaque, gpa: Allocator) Allocator.Error![]plan_mod.VolumeRow,
        /// The whole layout census (every entry of every live file).
        refs: *const fn (ctx: *anyopaque, gpa: Allocator) Allocator.Error![]census.Ref,
        mover: *const fn (ctx: *anyopaque) mover.Seam,
        pack: *const fn (ctx: *anyopaque) pack_compact.PackSeam,
        chunk: *const fn (ctx: *anyopaque) u64,
        packingOn: *const fn (ctx: *anyopaque) bool,
        packMaxSlot: *const fn (ctx: *anyopaque) u64,
        /// `(vol_tag, block_idx)` of every open pack + the open room.
        openPacks: *const fn (ctx: *anyopaque, gpa: Allocator) Allocator.Error![]plan_mod.BlockKey,
        openRoomBytes: *const fn (ctx: *anyopaque) u64,
        drainReclaim: *const fn (ctx: *anyopaque) void,
    };
};

pub const DefragDataBody = struct {
    gpa: Allocator,
    seam: DataSeam,
    /// ONE ascending floor per file for the pass (the convergence
    /// invariant); bounded by the plan's size.
    floors: std.AutoHashMapUnmanaged(u64, u64) = .empty,

    pub fn deinit(self: *DefragDataBody) void {
        self.floors.deinit(self.gpa);
    }

    pub fn body(self: *DefragDataBody) fabric_mod.Body {
        return .{ .ctx = self, .pass = &pass };
    }

    fn pass(ctx: *anyopaque, io: Io, f: *Fabric, job: *JobCtl) fabric_mod.PassError!fabric_mod.PassOutcome {
        const self: *DefragDataBody = @ptrCast(@alignCast(ctx));
        const vt = self.seam.vtable;
        const sctx = self.seam.ctx;
        if (job.passes > fabric_mod.DEFRAG_MAX_PASSES) return .completed;
        vt.drainReclaim(sctx);
        const rows = try vt.rows(sctx, self.gpa);
        defer self.gpa.free(rows);
        const refs = try vt.refs(sctx, self.gpa);
        defer self.gpa.free(refs);
        const chunk = vt.chunk(sctx);
        var plan = plan_mod.planDefrag(self.gpa, rows, job.rec.arg0, refs, chunk) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.ScopeIneligible => blk: {
                log.err("defrag --data: vol {x} is not placement-eligible (draining, disabled or unknown) — refused", .{job.rec.arg0});
                break :blk error.ScopeIneligible;
            },
        };
        defer plan.deinit(self.gpa);
        f.setRemaining(io, job, plan.moves.len);
        if (plan.moves.len == 0) return .completed;
        self.floors.clearRetainingCapacity();
        var progressed: u64 = 0;
        for (plan.moves) |m| {
            const t0 = clock.monotonicNs();
            const pick: mover.DestPick = switch (m.kind) {
                .d2_locality => blk: {
                    const gop = try self.floors.getOrPut(self.gpa, m.ino);
                    if (!gop.found_existing) gop.value_ptr.* = 0;
                    break :blk .{ .backend_ascending = .{ .vol_tag = m.target_vol, .floor = gop.value_ptr } };
                },
                .d1_compact => .compact_low,
            };
            const out = mover.moveOne(vt.mover(sctx), self.gpa, m.task, pick, 0) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                else => error.Io,
            };
            if (out == .moved) {
                progressed += 1;
                stats.defrag_blocks_moved.inc();
                stats.defrag_bytes_moved.add(chunk);
            }
            if (f.taskDone(io, job, clock.monotonicNs() - t0) == .stop) return .progressed;
        }
        return if (progressed > 0) .progressed else .completed;
    }
};

pub const DefragPackBody = struct {
    gpa: Allocator,
    seam: DataSeam,

    pub fn body(self: *DefragPackBody) fabric_mod.Body {
        return .{ .ctx = self, .pass = &pass };
    }

    fn pass(ctx: *anyopaque, io: Io, f: *Fabric, job: *JobCtl) fabric_mod.PassError!fabric_mod.PassOutcome {
        const self: *DefragPackBody = @ptrCast(@alignCast(ctx));
        const vt = self.seam.vtable;
        const sctx = self.seam.ctx;
        if (!vt.packingOn(sctx)) {
            log.err("defrag --pack: SQUEEZEFS_SMALL_FILE_PACKING is off on this mount — the whole arm is refused (an adopted lever-ON-era record included)", .{});
            return error.LeverOff;
        }
        if (job.passes > fabric_mod.DEFRAG_MAX_PASSES) return .completed;
        vt.drainReclaim(sctx);
        const rows = try vt.rows(sctx, self.gpa);
        defer self.gpa.free(rows);
        var scope: std.ArrayList(u64) = .empty;
        defer scope.deinit(self.gpa);
        for (rows) |r| if (r.eligible and (job.rec.arg0 == 0 or job.rec.arg0 == r.vol_tag)) try scope.append(self.gpa, r.vol_tag);
        if (scope.items.len == 0) return error.ScopeIneligible;
        const refs = try vt.refs(sctx, self.gpa);
        defer self.gpa.free(refs);
        const open = try vt.openPacks(sctx, self.gpa);
        defer self.gpa.free(open);
        const chunk = vt.chunk(sctx);
        var plan = plan_mod.planPackCompaction(self.gpa, .{ .refs = refs, .scope = scope.items, .chunk = chunk, .max_slot = vt.packMaxSlot(sctx), .open = open, .open_room_bytes = vt.openRoomBytes(sctx) }) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.ScopeIneligible => error.ScopeIneligible,
        };
        defer plan.deinit(self.gpa);
        f.setRemaining(io, job, plan.victims.len);
        if (!plan.executes()) return .completed;
        var compacted: usize = 0;
        for (plan.victims) |v| {
            const t0 = clock.monotonicNs();
            switch (pack_compact.compactOne(vt.pack(sctx), self.gpa, v) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.Io => error.Io,
            }) {
                .compacted => {
                    compacted += 1;
                    stats.pack_compactions.inc();
                    stats.pack_compaction_bytes_reclaimed.add(chunk - v.live_bytes);
                },
                .deferred, .superseded => {},
            }
            if (f.taskDone(io, job, clock.monotonicNs() - t0) == .stop) return .progressed;
        }
        return if (compacted > 0) .progressed else .completed;
    }
};

/// D3: the mount's fold targets and the kick (`FoldSeam`).
pub const FoldSeam = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        targets: *const fn (ctx: *anyopaque, gpa: Allocator) Allocator.Error![]u64,
        kick: *const fn (ctx: *anyopaque, io: Io, ino: u64) bool,
    };
};

pub const DefragFoldBody = struct {
    gpa: Allocator,
    seam: FoldSeam,

    pub fn body(self: *DefragFoldBody) fabric_mod.Body {
        return .{ .ctx = self, .pass = &pass };
    }

    /// Every target kicked once; one pass.
    fn pass(ctx: *anyopaque, io: Io, f: *Fabric, job: *JobCtl) fabric_mod.PassError!fabric_mod.PassOutcome {
        const self: *DefragFoldBody = @ptrCast(@alignCast(ctx));
        const targets = try self.seam.vtable.targets(self.seam.ctx, self.gpa);
        defer self.gpa.free(targets);
        f.setRemaining(io, job, targets.len);
        for (targets) |ino| {
            const t0 = clock.monotonicNs();
            if (self.seam.vtable.kick(self.seam.ctx, io, ino)) stats.defrag_folds_kicked.inc();
            if (f.taskDone(io, job, clock.monotonicNs() - t0) == .stop) return .progressed;
        }
        return .completed;
    }
};

/// D4: per meta volume the leaf compaction nudge and the merge sweep lap.
pub const MetaSeam = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        volumes: *const fn (ctx: *anyopaque) u32,
        /// Nudge up to `n` dead-bset candidates on volume `v`; the count kicked.
        nudge: *const fn (ctx: *anyopaque, io: Io, v: u32, n: u32) u32,
        /// ONE whole lap of the merge sweep on volume `v`; merges done.
        mergeLap: *const fn (ctx: *anyopaque, io: Io, v: u32) u64,
    };
};

pub const DefragMetaBody = struct {
    gpa: Allocator,
    seam: MetaSeam,

    pub fn body(self: *DefragMetaBody) fabric_mod.Body {
        return .{ .ctx = self, .pass = &pass };
    }

    /// One lap per volume: nudges in chunks of 8 (duty-parked between),
    /// then the merge sweep lap.
    fn pass(ctx: *anyopaque, io: Io, f: *Fabric, job: *JobCtl) fabric_mod.PassError!fabric_mod.PassOutcome {
        const self: *DefragMetaBody = @ptrCast(@alignCast(ctx));
        const vt = self.seam.vtable;
        const n = vt.volumes(self.seam.ctx);
        f.setRemaining(io, job, n);
        var v: u32 = 0;
        while (v < n) : (v += 1) {
            while (true) {
                const t0 = clock.monotonicNs();
                const kicked = vt.nudge(self.seam.ctx, io, v, META_NUDGE_CHUNK);
                stats.defrag_meta_compactions_kicked.add(kicked);
                if (kicked == 0) break;
                if (f.taskDone(io, job, clock.monotonicNs() - t0) == .stop) return .progressed;
            }
            const t1 = clock.monotonicNs();
            stats.defrag_meta_merges.add(vt.mergeLap(self.seam.ctx, io, v));
            if (f.taskDone(io, job, clock.monotonicNs() - t1) == .stop) return .progressed;
        }
        return .completed;
    }
};

// ---------------------------------------------------------------------------
// Tests over the RAM worlds.
// ---------------------------------------------------------------------------

const testing = std.testing;
const records = @import("../jobs/records.zig");

var seed: std.atomic.Value(u64) = .init(0xdef7a6);
fn rnd() u64 {
    return seed.fetchAdd(0x9E37_79B9_7F4A_7C15, .monotonic) | 1;
}

const CHUNK: u64 = 64 << 10;

const RamData = struct {
    gpa: Allocator,
    w: mover.RamWorld,
    pw: pack_compact.RamPackWorld,
    packing: bool = true,
    reclaim_drains: u64 = 0,

    fn init(gpa: Allocator) !*RamData {
        const d = try gpa.create(RamData);
        d.* = .{ .gpa = gpa, .w = mover.RamWorld.init(gpa, CHUNK), .pw = undefined };
        try d.w.addVolume(0xA, 16);
        try d.w.addVolume(0xB, 16);
        d.pw = .{ .w = &d.w, .open_vol = 0xA, .chunk = CHUNK };
        return d;
    }

    fn deinit(self: *RamData) void {
        self.w.deinit();
        self.gpa.destroy(self);
    }

    fn seam(self: *RamData) DataSeam {
        return .{ .ctx = self, .vtable = &vt };
    }

    const vt: DataSeam.VTable = .{ .rows = &rows, .refs = &refs, .mover = &moverFn, .pack = &packFn, .chunk = &chunk, .packingOn = &packingOn, .packMaxSlot = &packMaxSlot, .openPacks = &openPacks, .openRoomBytes = &openRoom, .drainReclaim = &drainReclaim };

    fn self_(ctx: *anyopaque) *RamData {
        return @ptrCast(@alignCast(ctx));
    }
    fn rows(ctx: *anyopaque, gpa: Allocator) Allocator.Error![]plan_mod.VolumeRow {
        const s = self_(ctx);
        const out = try gpa.alloc(plan_mod.VolumeRow, s.w.vols.count());
        for (s.w.vols.keys(), out) |tag, *r| r.* = .{ .vol_tag = tag, .eligible = s.w.eligible.contains(tag), .frontier = s.w.usedBlocks(tag) };
        return out;
    }
    fn refs(ctx: *anyopaque, gpa: Allocator) Allocator.Error![]census.Ref {
        const s = self_(ctx);
        var out: std.ArrayList(census.Ref) = .empty;
        var it = s.w.bindings.iterator();
        while (it.next()) |kv| try out.append(gpa, .{ .ino = kv.key_ptr.ino, .block_index = kv.key_ptr.block_index, .mapping = kv.value_ptr.* });
        return out.toOwnedSlice(gpa);
    }
    fn moverFn(ctx: *anyopaque) mover.Seam {
        return self_(ctx).w.seam();
    }
    fn packFn(ctx: *anyopaque) pack_compact.PackSeam {
        return self_(ctx).pw.seam();
    }
    fn chunk(_: *anyopaque) u64 {
        return CHUNK;
    }
    fn packingOn(ctx: *anyopaque) bool {
        return self_(ctx).packing;
    }
    fn packMaxSlot(_: *anyopaque) u64 {
        return CHUNK / 2;
    }
    fn openPacks(ctx: *anyopaque, gpa: Allocator) Allocator.Error![]plan_mod.BlockKey {
        const s = self_(ctx);
        if (s.pw.open) |o| {
            const out = try gpa.alloc(plan_mod.BlockKey, 1);
            out[0] = .{ .vol_tag = o.vol_tag, .block_idx = o.idx };
            return out;
        }
        return &.{};
    }
    fn openRoom(ctx: *anyopaque) u64 {
        const s = self_(ctx);
        const o = s.pw.open orelse return 0;
        return mover.RamWorld.DATA - o.next;
    }
    fn drainReclaim(ctx: *anyopaque) void {
        self_(ctx).reclaim_drains += 1;
    }
};

const Rig = struct {
    d: *RamData,
    store: fabric_mod.RamStore,
    fabric: Fabric,
    data_body: DefragDataBody,
    pack_body: DefragPackBody,

    fn init(gpa: Allocator) !*Rig {
        const r = try gpa.create(Rig);
        r.* = .{ .d = try RamData.init(gpa), .store = fabric_mod.RamStore.init(gpa), .fabric = undefined, .data_body = undefined, .pack_body = undefined };
        r.fabric = Fabric.init(gpa, r.store.store(), .{ .identity = .{ .node_token = 1, .mount_slot = 1 }, .pid = 1, .random = &rnd });
        r.data_body = .{ .gpa = gpa, .seam = r.d.seam() };
        r.pack_body = .{ .gpa = gpa, .seam = r.d.seam() };
        r.fabric.registerBody(.defrag_data, r.data_body.body());
        r.fabric.registerBody(.defrag_pack, r.pack_body.body());
        return r;
    }

    fn deinit(self: *Rig, gpa: Allocator) void {
        self.data_body.deinit();
        self.fabric.deinit();
        self.store.deinit();
        self.d.deinit();
        gpa.destroy(self);
    }
};

test "defrag --data: a scattered tail (contiguity ≤ 0.3) compacts below the frontier in one pass and the next plan finds nothing; a D2 streaming fixture ends same-backend ascending; bytes identical" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa);
    defer r.deinit(gpa);
    var data: [mover.RamWorld.DATA]u8 = @splat(0x11);
    // Vol A: blocks at 0, 3, 6, 9, 12 used by ino 1 (logical 0..4) — a
    // scattered tail (contiguity 2/8 = 0.25); frontier = 5.
    for ([_]u64{ 0, 3, 6, 9, 12 }, 0..) |idx, k| {
        data[0] = @intCast(k);
        _ = try r.d.w.plant(0xA, idx, &data, &.{.{ .ino = 1, .block_index = @intCast(k) }});
    }
    const F = struct {
        fn isFree(w: *mover.RamWorld, i: u64) bool {
            return w.block(0xA, i).state == .free;
        }
    };
    const before = metrics.measureD1(0xA, 13, &r.d.w, F.isFree);
    try testing.expect(before.contiguityPermille() <= 300);
    const id = try r.fabric.submit(io, .{ .job_type = .defrag_data, .arg0 = 0xA });
    const j = r.fabric.claimNext(io, false).?;
    try testing.expectEqual(records.JobState.completed, r.fabric.runClaimed(io, j));
    try testing.expectEqual(@as(u64, 5), r.d.w.usedBlocks(0xA));
    for (0..5) |i| try testing.expect(r.d.w.block(0xA, i).state == .allocated);
    for (0..5) |k| try testing.expectEqual(@as(u8, @intCast(k)), r.d.w.resolve(.{ .ino = 1, .block_index = @intCast(k) }).?[0]);
    const after = metrics.measureD1(0xA, 5, &r.d.w, F.isFree);
    try testing.expectEqual(@as(u64, 1000), after.contiguityPermille());
    // The passes converge to same-backend ASCENDING below the frontier
    // (the D1 arm's gap consumption left logical 1 at idx 3 out of order;
    // the D2 arm then re-laid the file and D1 compacted it again).
    for (0..4) |k| try testing.expect(r.d.w.bindings.get(.{ .ino = 1, .block_index = @intCast(k) }).?.blockIdx(CHUNK) < r.d.w.bindings.get(.{ .ino = 1, .block_index = @intCast(k + 1) }).?.blockIdx(CHUNK));
    try testing.expect(r.store.record(id).?.tasks_done >= 3);
    const id2 = try r.fabric.submit(io, .{ .job_type = .defrag_data });
    try testing.expectEqual(records.JobState.completed, r.fabric.runClaimed(io, r.fabric.claimNext(io, false).?));
    try testing.expectEqual(@as(u64, 0), r.store.record(id2).?.tasks_done);
    // D2: ino 2 striped A, B, A → plurality A; after the pass same-backend ascending.
    _ = try r.d.w.plant(0xA, 5, &data, &.{.{ .ino = 2, .block_index = 0 }});
    _ = try r.d.w.plant(0xB, 7, &data, &.{.{ .ino = 2, .block_index = 1 }});
    _ = try r.d.w.plant(0xA, 6, &data, &.{.{ .ino = 2, .block_index = 2 }});
    _ = try r.fabric.submit(io, .{ .job_type = .defrag_data });
    try testing.expectEqual(records.JobState.completed, r.fabric.runClaimed(io, r.fabric.claimNext(io, false).?));
    const m0 = r.d.w.bindings.get(.{ .ino = 2, .block_index = 0 }).?;
    const m1 = r.d.w.bindings.get(.{ .ino = 2, .block_index = 1 }).?;
    const m2 = r.d.w.bindings.get(.{ .ino = 2, .block_index = 2 }).?;
    try testing.expect(m0.vol_tag == 0xA and m1.vol_tag == 0xA and m2.vol_tag == 0xA);
    try testing.expect(m0.blockIdx(CHUNK) < m1.blockIdx(CHUNK) and m1.blockIdx(CHUNK) < m2.blockIdx(CHUNK));
    try testing.expectEqual(@as(u64, 0), r.d.w.usedBlocks(0xB));
    // Naming a draining volume fails loud.
    _ = r.d.w.eligible.remove(0xB);
    const id3 = try r.fabric.submit(io, .{ .job_type = .defrag_data, .arg0 = 0xB });
    log.testing.expectErrorLogs(2);
    try testing.expectEqual(records.JobState.failed, r.fabric.runClaimed(io, r.fabric.claimNext(io, false).?));
    try log.testing.expectAllFired();
    try testing.expectEqual(records.ErrorClass.scope_ineligible, r.store.record(id3).?.error_class);
}

test "defrag --pack on half-empty packs: fewer blocks, every file byte-exact, the record counts the victims; the lever off gates the whole arm loud; report-only moves nothing" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa);
    defer r.deinit(gpa);
    var data: [mover.RamWorld.DATA]u8 = undefined;
    for (&data, 0..) |*d, i| d.* = @intCast(i * 3);
    // Three packs with 10 B tenants at 0/20/40 (the fake's slots).
    var ino: u64 = 1;
    for ([_]u64{ 0, 1, 2 }) |idx| {
        _ = try r.d.w.plant(0xA, idx, &data, &.{});
        for ([_]u32{ 0, 20 }) |off| {
            _ = try r.d.w.plantTenant(0xA, idx, off, 10, .{ .ino = ino, .block_index = 0 });
            ino += 1;
        }
    }
    try testing.expectEqual(@as(u64, 3), r.d.w.usedBlocks(0xA));
    // Report-only (the planner alone) moves nothing.
    const refs0 = try RamData.refs(r.d, gpa);
    defer gpa.free(refs0);
    var p0 = try plan_mod.planPackCompaction(gpa, .{ .refs = refs0, .scope = &.{0xA}, .chunk = CHUNK, .max_slot = CHUNK / 2, .open = &.{}, .open_room_bytes = 0 });
    defer p0.deinit(gpa);
    try testing.expectEqual(@as(usize, 3), p0.victims.len);
    try testing.expectEqual(@as(u64, 3), r.d.w.usedBlocks(0xA));
    // The lever off refuses the whole arm.
    r.d.packing = false;
    const off_id = try r.fabric.submit(io, .{ .job_type = .defrag_pack });
    log.testing.expectErrorLogs(2);
    try testing.expectEqual(records.JobState.failed, r.fabric.runClaimed(io, r.fabric.claimNext(io, false).?));
    try log.testing.expectAllFired();
    try testing.expectEqual(records.ErrorClass.lever_off, r.store.record(off_id).?.error_class);
    r.d.packing = true;
    const id = try r.fabric.submit(io, .{ .job_type = .defrag_pack });
    try testing.expectEqual(records.JobState.completed, r.fabric.runClaimed(io, r.fabric.claimNext(io, false).?));
    r.d.pw.sealOpen();
    // 6 tenants × 10 B = 60 B fit one 64 B fake pack: 3 → 1 block.
    try testing.expectEqual(@as(u64, 1), r.d.w.usedBlocks(0xA));
    try testing.expect(r.store.record(id).?.tasks_done >= 3);
    var k: u64 = 1;
    while (k <= 6) : (k += 1) {
        const off: usize = if ((k - 1) % 2 == 0) 0 else 20;
        try testing.expectEqualSlices(u8, data[off .. off + 10], r.d.w.resolve(.{ .ino = k, .block_index = 0 }).?);
    }
}

test "defrag --fold kicks every target once; defrag --meta nudges in chunks of 8 then runs one merge lap per volume" {
    const gpa = testing.allocator;
    const io = testing.io;
    const Fold = struct {
        kicked: std.ArrayList(u64) = .empty,
        fn targets(_: *anyopaque, g: Allocator) Allocator.Error![]u64 {
            return g.dupe(u64, &.{ 7, 8, 9 });
        }
        fn kick(ctx: *anyopaque, _: Io, ino: u64) bool {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.kicked.append(gpa, ino) catch return false;
            return true;
        }
        const vt: FoldSeam.VTable = .{ .targets = &targets, .kick = &kick };
    };
    var fold: Fold = .{};
    defer fold.kicked.deinit(gpa);
    const Meta = struct {
        candidates: [2]u32 = .{ 19, 3 },
        laps: u32 = 0,
        fn volumes(_: *anyopaque) u32 {
            return 2;
        }
        fn nudge(ctx: *anyopaque, _: Io, v: u32, n: u32) u32 {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            const take = @min(n, self.candidates[v]);
            self.candidates[v] -= take;
            return take;
        }
        fn mergeLap(ctx: *anyopaque, _: Io, _: u32) u64 {
            const self: *@This() = @ptrCast(@alignCast(ctx));
            self.laps += 1;
            return 4;
        }
        const vt: MetaSeam.VTable = .{ .volumes = &volumes, .nudge = &nudge, .mergeLap = &mergeLap };
    };
    var meta: Meta = .{};
    var store = fabric_mod.RamStore.init(gpa);
    defer store.deinit();
    var f = Fabric.init(gpa, store.store(), .{ .identity = .{ .node_token = 1, .mount_slot = 1 }, .pid = 1, .random = &rnd });
    defer f.deinit();
    var fb: DefragFoldBody = .{ .gpa = gpa, .seam = .{ .ctx = &fold, .vtable = &Fold.vt } };
    var mb: DefragMetaBody = .{ .gpa = gpa, .seam = .{ .ctx = &meta, .vtable = &Meta.vt } };
    f.registerBody(.defrag_fold, fb.body());
    f.registerBody(.defrag_meta, mb.body());
    const kicks0 = stats.defrag_folds_kicked.load();
    _ = try f.submit(io, .{ .job_type = .defrag_fold });
    try testing.expectEqual(records.JobState.completed, f.runClaimed(io, f.claimNext(io, false).?));
    try testing.expectEqualSlices(u64, &.{ 7, 8, 9 }, fold.kicked.items);
    try testing.expectEqual(kicks0 + 3, stats.defrag_folds_kicked.load());
    const comp0 = stats.defrag_meta_compactions_kicked.load();
    const merges0 = stats.defrag_meta_merges.load();
    _ = try f.submit(io, .{ .job_type = .defrag_meta });
    try testing.expectEqual(records.JobState.completed, f.runClaimed(io, f.claimNext(io, false).?));
    try testing.expectEqual(comp0 + 22, stats.defrag_meta_compactions_kicked.load());
    try testing.expectEqual(merges0 + 8, stats.defrag_meta_merges.load());
    try testing.expectEqual(@as(u32, 2), meta.laps);
    try testing.expectEqual(@as(u32, 8), META_NUDGE_CHUNK);
}
