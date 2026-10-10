//! The mover JOB BODIES the fabric runs (spec §1.4, §2.4, §2.5.4):
//! `evacuate_volume` (the drain: re-plan every pass from the census, the
//! capacity re-verify at every checkpoint, retire on convergence) and
//! `rebalance` (ONE bounded pass of `plan_rebalance` legs). Both are
//! PORTABLE over the census walk, the mover seam and a `DrainSeam` the
//! plane binds (the volume state, the used-block count, the survivor
//! rows, the reclaim drain, the retire); the kill matrix runs on every
//! host over the RAM worlds. Correctness is plan regeneration (KD-6): a
//! pass never trusts the previous pass's bookkeeping.
//!
//! Per pass: drain the reclaim queue FIRST (planning against queued-not-
//! finished frees defers spuriously and never converges); re-plan;
//! publish `tasks_total = done + remaining`; the capacity re-verify (a
//! refusal parks `PausedCapacity`, loud); an empty census ⇒ drain reclaim
//! again (space RETURNED, not queued) ⇒ `victim.used_blocks == 0` ⇒ retire
//! ⇒ Completed, else "census empty but the victim still holds N blocks —
//! re-planning" (deferred, `REPLAN_BACKOFF`). A victim no longer
//! `Draining` ends the evacuation as cancelled (undrain raced it). The
//! closure `moved + superseded + remaining ≡ distinct_blocks` holds at
//! every re-plan.

const std = @import("std");
const Io = std.Io;
const Allocator = std.mem.Allocator;
const log = @import("../../core/log.zig");
const clock = @import("../../core/clock.zig");
const set_config = @import("../../data/set_config.zig");
const fabric_mod = @import("../jobs/fabric.zig");
const preflight = @import("../jobs/preflight.zig");
const rebalance = @import("../jobs/rebalance.zig");
const throttle = @import("../jobs/throttle.zig");
const census = @import("census.zig");
const mover = @import("mover.zig");
const stats_mod = @import("stats.zig");

const stats = stats_mod.stats;
const Fabric = fabric_mod.Fabric;
const JobCtl = fabric_mod.JobCtl;

/// What the drain needs of the plane beyond the walk and the mover.
pub const DrainSeam = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        walk: *const fn (ctx: *anyopaque) census.LayoutWalk,
        mover: *const fn (ctx: *anyopaque) mover.Seam,
        /// The durable lifecycle state of `vol_tag` (null = unknown).
        volumeState: *const fn (ctx: *anyopaque, vol_tag: u64) ?set_config.State,
        usedBlocks: *const fn (ctx: *anyopaque, vol_tag: u64) u64,
        /// Every data volume's row (the preflight sums the survivors).
        rows: *const fn (ctx: *anyopaque, gpa: Allocator) Allocator.Error![]preflight.SurvivorRow,
        drainReclaim: *const fn (ctx: *anyopaque) void,
        /// The retire tx + the runtime deregistration after it.
        retire: *const fn (ctx: *anyopaque, vol_tag: u64) error{Io}!void,
        blockSize: *const fn (ctx: *anyopaque) u64,
        writeRateBps: *const fn (ctx: *anyopaque) u64,
        workers: *const fn (ctx: *anyopaque) u64,
    };
};

/// The drain body: one per fabric.
pub const DrainBody = struct {
    gpa: Allocator,
    seam: DrainSeam,
    /// The measured job rate from the first checkpoint on.
    moved_bytes: u64 = 0,
    started_ns: u64 = 0,

    pub fn body(self: *DrainBody) fabric_mod.Body {
        return .{ .ctx = self, .pass = &pass };
    }

    fn measuredRateBps(self: *const DrainBody) u64 {
        if (self.started_ns == 0 or self.moved_bytes == 0) return 0;
        const el = clock.monotonicNs() -| self.started_ns;
        if (el == 0) return 0;
        return @intCast(@as(u128, self.moved_bytes) * std.time.ns_per_s / el);
    }

    fn pass(ctx: *anyopaque, io: Io, f: *Fabric, job: *JobCtl) fabric_mod.PassError!fabric_mod.PassOutcome {
        const self: *DrainBody = @ptrCast(@alignCast(ctx));
        const vt = self.seam.vtable;
        const sctx = self.seam.ctx;
        const victim = job.rec.arg0;
        if (self.started_ns == 0) self.started_ns = clock.monotonicNs();
        defer stats_mod.clearDrainGauges();
        if (vt.volumeState(sctx, victim) != .draining) {
            log.info("drain of vol {x}: the volume is no longer Draining — the evacuation ends as cancelled (an undrain won)", .{victim});
            return .superseded;
        }
        vt.drainReclaim(sctx);
        var plan = census.censusFor(self.gpa, vt.walk(sctx), &.{victim}, vt.blockSize(sctx)) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
        };
        defer plan.deinit(self.gpa);
        stats.evacuate_replans.inc();
        f.setRemaining(io, job, plan.distinct_blocks);
        // The capacity re-verify at every checkpoint.
        const rows = try vt.rows(sctx, self.gpa);
        defer self.gpa.free(rows);
        const block = vt.blockSize(sctx);
        const pf = preflight.compute(.{ .needed = plan.distinct_blocks * block, .rows = rows, .victim = victim, .workers = vt.workers(sctx), .block_size = block, .write_rate_bps = vt.writeRateBps(sctx), .measured_job_rate_bps = self.measuredRateBps() });
        stats_mod.publishDrainGauges(pf.needed, pf.avail, pf.transient);
        if (pf.refuses()) {
            var buf: [256]u8 = undefined;
            var w: Io.Writer = .fixed(&buf);
            pf.write(&w) catch {};
            log.warn("drain of vol {x}: {s}", .{ victim, w.buffered() });
            stats.volume_preflight_refusals.inc();
            return .paused_capacity;
        }
        if (plan.tasks.len == 0) {
            vt.drainReclaim(sctx);
            const used = vt.usedBlocks(sctx, victim);
            if (used == 0) {
                vt.retire(sctx, victim) catch return error.Io;
                stats.volume_retired.inc();
                log.info("drain of vol {x}: census empty and the victim holds no allocated block — retired", .{victim});
                return .completed;
            }
            log.info("drain of vol {x}: census empty but the victim still holds {d} allocated block(s) (in-flight writes or leaks pending remount reclaim) — re-planning", .{ victim, used });
            return .deferred;
        }
        var moved: u64 = 0;
        var superseded: u64 = 0;
        var deferred: u64 = 0;
        const pct = job.ctl.loadPct();
        _ = throttle.pipelineWidth(pct, false);
        for (plan.tasks, 0..) |t, i| {
            const t0 = clock.monotonicNs();
            const out = mover.moveOne(vt.mover(sctx), self.gpa, t, .{ .balance = .{ .dest_hint = t.dest_hint } }, victim) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.Io, error.Killed => error.Io,
            };
            switch (out) {
                .moved => {
                    moved += 1;
                    self.moved_bytes += block;
                },
                .superseded => superseded += 1,
                .deferred => deferred += 1,
            }
            // The closure at every step: what is left is what the re-plan
            // would find.
            std.debug.assert(moved + superseded + deferred + (plan.tasks.len - i - 1) == plan.distinct_blocks);
            if (f.taskDone(io, job, clock.monotonicNs() - t0) == .stop) return .progressed;
        }
        // Progress is a MOVE. A pass of superseded tasks alone re-plans
        // after the backoff like a deferred one: the census it planned from
        // was stale (a writer moved the block, or a republish refused), and
        // an immediate re-plan on a persistently refused block was a hot
        // loop (17,240 passes in two minutes, rung 8).
        return if (moved > 0) .progressed else .deferred;
    }
};

/// The rebalance body: ONE bounded pass then Completed; deferred or
/// superseded tasks are not retried (the router's balance penalty is the
/// continuous drift prevention).
pub const RebalanceBody = struct {
    gpa: Allocator,
    seam: DrainSeam,

    pub fn body(self: *RebalanceBody) fabric_mod.Body {
        return .{ .ctx = self, .pass = &pass };
    }

    fn pass(ctx: *anyopaque, io: Io, f: *Fabric, job: *JobCtl) fabric_mod.PassError!fabric_mod.PassOutcome {
        const self: *RebalanceBody = @ptrCast(@alignCast(ctx));
        const vt = self.seam.vtable;
        const sctx = self.seam.ctx;
        vt.drainReclaim(sctx);
        const rows = try vt.rows(sctx, self.gpa);
        defer self.gpa.free(rows);
        var rrows = try self.gpa.alloc(rebalance.Row, rows.len);
        defer self.gpa.free(rrows);
        var n: usize = 0;
        for (rows) |r| if (r.eligible) {
            rrows[n] = .{ .vol_tag = r.vol_tag, .used_bytes = r.used_bytes, .capacity_bytes = r.capacity_bytes };
            n += 1;
        };
        const block = vt.blockSize(sctx);
        const legs = try rebalance.planLegs(self.gpa, rrows[0..n], block);
        defer self.gpa.free(legs);
        if (legs.len == 0) return .completed;
        var sources: std.ArrayList(u64) = .empty;
        defer sources.deinit(self.gpa);
        for (legs) |l| {
            var seen = false;
            for (sources.items) |s| seen = seen or s == l.src;
            if (!seen) try sources.append(self.gpa, l.src);
        }
        var plan = census.censusFor(self.gpa, vt.walk(sctx), sources.items, block) catch |e| return switch (e) {
            error.OutOfMemory => error.OutOfMemory,
            error.Io => error.Io,
        };
        defer plan.deinit(self.gpa);
        try census.boundToLegs(self.gpa, &plan, legs);
        f.setRemaining(io, job, plan.tasks.len);
        for (plan.tasks) |t| {
            const t0 = clock.monotonicNs();
            _ = mover.moveOne(vt.mover(sctx), self.gpa, t, .{ .balance = .{ .dest_hint = t.dest_hint } }, 0) catch |e| return switch (e) {
                error.OutOfMemory => error.OutOfMemory,
                error.Io, error.Killed => error.Io,
            };
            if (f.taskDone(io, job, clock.monotonicNs() - t0) == .stop) return .progressed;
        }
        return .completed;
    }
};

// ---------------------------------------------------------------------------
// The RAM drain world + tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const records = @import("../jobs/records.zig");

pub const RamDrainWorld = struct {
    gpa: Allocator,
    w: mover.RamWorld,
    walk_refs: std.ArrayList(census.Ref) = .empty,
    states: std.AutoHashMapUnmanaged(u64, set_config.State) = .empty,
    reclaim_drains: u64 = 0,
    retired: std.ArrayList(u64) = .empty,
    write_rate: u64 = 0,
    /// The rows' capacity: large enough that the 1 GiB headroom floor
    /// never refuses a tiny RAM world (a test narrows it to force one).
    capacity_blocks: u64 = 1 << 20,

    pub fn init(gpa: Allocator, chunk: u64) RamDrainWorld {
        return .{ .gpa = gpa, .w = mover.RamWorld.init(gpa, chunk) };
    }

    pub fn deinit(self: *RamDrainWorld) void {
        self.w.deinit();
        self.walk_refs.deinit(self.gpa);
        self.states.deinit(self.gpa);
        self.retired.deinit(self.gpa);
    }

    pub fn seam(self: *RamDrainWorld) DrainSeam {
        return .{ .ctx = self, .vtable = &vt };
    }

    const vt: DrainSeam.VTable = .{ .walk = &walk, .mover = &moverFn, .volumeState = &volumeState, .usedBlocks = &usedBlocks, .rows = &rows, .drainReclaim = &drainReclaim, .retire = &retire, .blockSize = &blockSize, .writeRateBps = &writeRate, .workers = &workers };

    fn self_(ctx: *anyopaque) *RamDrainWorld {
        return @ptrCast(@alignCast(ctx));
    }
    fn walk(ctx: *anyopaque) census.LayoutWalk {
        return .{ .ctx = ctx, .walk = &runWalk };
    }
    fn runWalk(ctx: *anyopaque, v: census.LayoutWalk.Visitor) error{ Io, OutOfMemory }!void {
        const s = self_(ctx);
        var it = s.w.bindings.iterator();
        while (it.next()) |kv| try v.visit(v.ctx, .{ .ino = kv.key_ptr.ino, .block_index = kv.key_ptr.block_index, .mapping = kv.value_ptr.* });
    }
    fn moverFn(ctx: *anyopaque) mover.Seam {
        return self_(ctx).w.seam();
    }
    fn volumeState(ctx: *anyopaque, vol_tag: u64) ?set_config.State {
        return self_(ctx).states.get(vol_tag);
    }
    fn usedBlocks(ctx: *anyopaque, vol_tag: u64) u64 {
        return self_(ctx).w.usedBlocks(vol_tag);
    }
    fn rows(ctx: *anyopaque, gpa: Allocator) Allocator.Error![]preflight.SurvivorRow {
        const s = self_(ctx);
        const out = try gpa.alloc(preflight.SurvivorRow, s.w.vols.count());
        for (s.w.vols.keys(), out) |tag, *r| {
            const st = s.states.get(tag) orelse .active;
            r.* = .{ .vol_tag = tag, .eligible = st == .active, .capacity_bytes = s.capacity_blocks * s.w.chunk, .used_bytes = s.w.usedBlocks(tag) * s.w.chunk };
        }
        return out;
    }
    fn drainReclaim(ctx: *anyopaque) void {
        self_(ctx).reclaim_drains += 1;
    }
    fn retire(ctx: *anyopaque, vol_tag: u64) error{Io}!void {
        const s = self_(ctx);
        s.states.put(s.gpa, vol_tag, .retired) catch return error.Io;
        s.retired.append(s.gpa, vol_tag) catch return error.Io;
    }
    fn blockSize(ctx: *anyopaque) u64 {
        return self_(ctx).w.chunk;
    }
    fn writeRate(ctx: *anyopaque) u64 {
        return self_(ctx).write_rate;
    }
    fn workers(_: *anyopaque) u64 {
        return 1;
    }
};

var seed: std.atomic.Value(u64) = .init(0xd4a1);
fn rnd() u64 {
    return seed.fetchAdd(0x9E37_79B9_7F4A_7C15, .monotonic) | 1;
}

const CHUNK: u64 = 4096;

const Rig = struct {
    dw: RamDrainWorld,
    store: fabric_mod.RamStore,
    fabric: Fabric,
    drain: DrainBody,
    rebal: RebalanceBody,

    fn init(gpa: Allocator) !*Rig {
        const r = try gpa.create(Rig);
        r.* = .{ .dw = RamDrainWorld.init(gpa, CHUNK), .store = fabric_mod.RamStore.init(gpa), .fabric = undefined, .drain = undefined, .rebal = undefined };
        try r.dw.w.addVolume(0xA, 8);
        try r.dw.w.addVolume(0xB, 8);
        try r.dw.w.addVolume(0xC, 8);
        try r.dw.states.put(gpa, 0xA, .active);
        try r.dw.states.put(gpa, 0xB, .active);
        try r.dw.states.put(gpa, 0xC, .active);
        r.fabric = Fabric.init(gpa, r.store.store(), .{ .identity = .{ .node_token = 1, .mount_slot = 1 }, .pid = 1, .random = &rnd });
        r.drain = .{ .gpa = gpa, .seam = r.dw.seam() };
        r.rebal = .{ .gpa = gpa, .seam = r.dw.seam() };
        r.fabric.registerBody(.evacuate_volume, r.drain.body());
        r.fabric.registerBody(.rebalance, r.rebal.body());
        return r;
    }

    fn deinit(self: *Rig, gpa: Allocator) void {
        self.fabric.deinit();
        self.store.deinit();
        self.dw.deinit();
        gpa.destroy(self);
    }

    fn plant(self: *Rig, vol: u64, idx: u64, fill: u8, owners: []const mover.RamWorld.BindKey) !void {
        var data: [mover.RamWorld.DATA]u8 = @splat(fill);
        _ = try self.dw.w.plant(vol, idx, &data, owners);
    }
};

test "drain evacuates, retires and preserves bytes: a clone-shared block moves once, the victim ends with zero used blocks and Retired, the record completes; the preflight numbers ride the gauges during the pass and clear after" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa);
    defer r.deinit(gpa);
    try r.plant(0xA, 0, 0x10, &.{.{ .ino = 1, .block_index = 0 }});
    try r.plant(0xA, 1, 0x11, &.{ .{ .ino = 2, .block_index = 0 }, .{ .ino = 3, .block_index = 0 } });
    try r.plant(0xA, 2, 0x12, &.{.{ .ino = 1, .block_index = 1 }});
    try r.plant(0xB, 0, 0x20, &.{.{ .ino = 4, .block_index = 0 }});
    try r.dw.states.put(gpa, 0xA, .draining);
    const id = try r.fabric.submit(io, .{ .job_type = .evacuate_volume, .arg0 = 0xA });
    const j = r.fabric.claimNext(io, false).?;
    const moved0 = stats.evacuate_blocks_moved.load();
    const shared0 = stats.evacuate_shared_blocks_moved.load();
    try testing.expectEqual(records.JobState.completed, r.fabric.runClaimed(io, j));
    try testing.expectEqual(moved0 + 3, stats.evacuate_blocks_moved.load());
    try testing.expectEqual(shared0 + 1, stats.evacuate_shared_blocks_moved.load());
    try testing.expectEqual(@as(u64, 0), r.dw.w.usedBlocks(0xA));
    try testing.expectEqual(@as(?set_config.State, .retired), r.dw.states.get(0xA));
    try testing.expectEqualSlices(u64, &.{0xA}, r.dw.retired.items);
    try testing.expectEqual(@as(u64, 3), r.store.record(id).?.tasks_done);
    try testing.expectEqual(@as(u64, 0), stats.evacuate_needed_bytes.load());
    try testing.expectEqual(@as(u64, 0), stats.evacuate_inflight_bytes.load());
    // Bytes intact.
    try testing.expectEqual(@as(u8, 0x11), r.dw.w.resolve(.{ .ino = 3, .block_index = 0 }).?[0]);
    try testing.expectEqual(@as(u8, 0x12), r.dw.w.resolve(.{ .ino = 1, .block_index = 1 }).?[0]);
    // The reclaim queue was drained before planning and before retiring.
    try testing.expect(r.dw.reclaim_drains >= 2);
}

test "remove-data refuses without survivor capacity printing every term: the drain parks PausedCapacity loud; a victim that is no longer Draining ends as cancelled" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa);
    defer r.deinit(gpa);
    try r.plant(0xA, 0, 0x10, &.{.{ .ino = 1, .block_index = 0 }});
    // Survivors with no room: every B/C block used, at their true size.
    r.dw.capacity_blocks = 8;
    var i: u64 = 0;
    while (i < 8) : (i += 1) {
        try r.plant(0xB, i, 0x20, &.{.{ .ino = 100 + i, .block_index = 0 }});
        try r.plant(0xC, i, 0x30, &.{.{ .ino = 200 + i, .block_index = 0 }});
    }
    try r.dw.states.put(gpa, 0xA, .draining);
    const id = try r.fabric.submit(io, .{ .job_type = .evacuate_volume, .arg0 = 0xA });
    const j = r.fabric.claimNext(io, false).?;
    const refusals0 = stats.volume_preflight_refusals.load();
    try testing.expectEqual(records.JobState.paused_capacity, r.fabric.runClaimed(io, j));
    try testing.expectEqual(refusals0 + 1, stats.volume_preflight_refusals.load());
    try testing.expect(r.store.record(id).?.capacity_pause);
    try testing.expectEqual(@as(u64, 1), r.dw.w.usedBlocks(0xA));
    // Undrain raced: the next pass self-cancels.
    try r.dw.states.put(gpa, 0xA, .active);
    try r.fabric.resumeJob(io, id);
    const j2 = r.fabric.claimNext(io, false).?;
    try testing.expectEqual(records.JobState.cancelled, r.fabric.runClaimed(io, j2));
}

test "a crash mid-drain resumes by re-plan and preserves bytes: the process dies inside a move; a fresh fabric over the same store adopts the Running record as Queued, re-censuses the remounted world and converges to Retired" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa);
    defer r.deinit(gpa);
    try r.plant(0xA, 0, 0x10, &.{.{ .ino = 1, .block_index = 0 }});
    try r.plant(0xA, 1, 0x11, &.{ .{ .ino = 2, .block_index = 0 }, .{ .ino = 3, .block_index = 0 } });
    try r.plant(0xA, 2, 0x12, &.{.{ .ino = 1, .block_index = 1 }});
    try r.dw.states.put(gpa, 0xA, .draining);
    const id = try r.fabric.submit(io, .{ .job_type = .evacuate_volume, .arg0 = 0xA });
    const j = r.fabric.claimNext(io, false).?;
    r.dw.w.kill_at = .mid_publish; // the shared block's second referencer
    // The pass dies (the process would): the fabric never finishes the job.
    try testing.expectError(error.Io, DrainBody.pass(&r.drain, io, &r.fabric, j));
    try testing.expect(r.dw.w.killed);
    try testing.expectEqual(records.JobState.running, r.store.record(id).?.state);
    // Remount + a new fabric over the same durable store.
    r.dw.w.remount();
    var f2 = Fabric.init(gpa, r.store.store(), .{ .identity = .{ .node_token = 2, .mount_slot = 1 }, .pid = 2, .random = &rnd });
    defer f2.deinit();
    var drain2: DrainBody = .{ .gpa = gpa, .seam = r.dw.seam() };
    f2.registerBody(.evacuate_volume, drain2.body());
    try testing.expectEqual(@as(usize, 1), try f2.adopt(io));
    try testing.expectEqual(records.JobState.queued, r.store.record(id).?.state);
    const j2 = f2.claimNext(io, false).?;
    try testing.expectEqual(records.JobState.completed, f2.runClaimed(io, j2));
    try testing.expectEqual(@as(u64, 0), r.dw.w.usedBlocks(0xA));
    try testing.expectEqual(@as(?set_config.State, .retired), r.dw.states.get(0xA));
    try testing.expectEqual(@as(u8, 0x11), r.dw.w.resolve(.{ .ino = 2, .block_index = 0 }).?[0]);
    try testing.expectEqual(@as(u8, 0x11), r.dw.w.resolve(.{ .ino = 3, .block_index = 0 }).?[0]);
    try testing.expectEqual(@as(u8, 0x10), r.dw.w.resolve(.{ .ino = 1, .block_index = 0 }).?[0]);
    try testing.expectEqual(@as(u8, 0x12), r.dw.w.resolve(.{ .ino = 1, .block_index = 1 }).?[0]);
    // Every binding resolves to exactly one live block (no duplicate, no loss).
    try testing.expectEqual(@as(usize, 4), r.dw.w.bindings.count());
}

test "rebalance moves toward the set mean: a 7/4/1-of-8 set ends within 10 pp in ONE pass and completes; a balanced set completes with nothing moved" {
    const gpa = testing.allocator;
    const io = testing.io;
    const r = try Rig.init(gpa);
    defer r.deinit(gpa);
    r.dw.capacity_blocks = 8;
    var i: u64 = 0;
    while (i < 7) : (i += 1) try r.plant(0xA, i, 0x10, &.{.{ .ino = 1, .block_index = @intCast(i) }});
    i = 0;
    while (i < 4) : (i += 1) try r.plant(0xB, i, 0x20, &.{.{ .ino = 2, .block_index = @intCast(i) }});
    try r.plant(0xC, 0, 0x30, &.{.{ .ino = 3, .block_index = 0 }});
    const id = try r.fabric.submit(io, .{ .job_type = .rebalance, .throttle_pct = 100 });
    const j = r.fabric.claimNext(io, false).?;
    try testing.expectEqual(records.JobState.completed, r.fabric.runClaimed(io, j));
    // mean = 12/24 = 50 %; floor 40 % = 3.2 blocks ⇒ C needs 2 blocks
    // (deficit (3.2 − 1) = 2); A's surplus 7 − 4 = 3 ⇒ 2 move A → C.
    try testing.expectEqual(@as(u64, 5), r.dw.w.usedBlocks(0xA));
    try testing.expectEqual(@as(u64, 4), r.dw.w.usedBlocks(0xB));
    try testing.expectEqual(@as(u64, 3), r.dw.w.usedBlocks(0xC));
    try testing.expectEqual(@as(u64, 2), r.store.record(id).?.tasks_done);
    for (0..7) |k| try testing.expectEqual(@as(u8, 0x10), r.dw.w.resolve(.{ .ino = 1, .block_index = @intCast(k) }).?[0]);
    const id2 = try r.fabric.submit(io, .{ .job_type = .rebalance });
    const j2 = r.fabric.claimNext(io, false).?;
    try testing.expectEqual(records.JobState.completed, r.fabric.runClaimed(io, j2));
    try testing.expectEqual(@as(u64, 0), r.store.record(id2).?.tasks_done);
}
