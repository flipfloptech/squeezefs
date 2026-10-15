//! Shard leases, fencing, expiry, quarantine (charter §8 "shard leases (30 s
//! TTL, 10 s heartbeat, fencing), result PROPOSALS only, the pre-allocated-
//! unpublished-destination law in both halves, coordinator WERO +
//! preempt-on-expiry"; spec §1.6). The coordinator-side table every remote
//! (and fleet read) shard rides, PORTABLE over two seams: the quarantine
//! sink (`custody.DestinationCohort` under a fresh dead epoch: admitted
//! and relinquished, its owed publishes finished at the release) and the
//! PR preempt (`guard/pr.zig` under the standing WERO).
//!
//! The law per shard: `assign` reuses the shard state with a BUMPED
//! fencing or starts at 0 — every assignment gets FRESH destinations (an
//! expired lease's tuples are quarantined; an allocator that returns a
//! quarantined tuple is an invariant breach refused loud); `renew` under
//! the current fencing extends the deadline a TTL; the sweeper ticks
//! `max(TTL/4, 25 ms)` and `expire`s what passed: fencing +1 →
//! `job_remote_lease_expiries` → the destinations enter the do-not-publish
//! quarantine under a fresh dead epoch (the live-zombie half: a paused
//! worker resuming DMA lands only on offsets nothing will reference or
//! reuse) → the holder is marked expired so a late submit is REFUSED, not
//! dropped → PR-preempt the host's registration (a landed preempt IS the
//! drain proof that releases the cohort; on a detection-grade substrate the
//! cohort stays quarantined) → requeue. A READ shard (no destinations)
//! takes the fencing/notify arm ONLY — no quarantine, NO PR preempt (a
//! read worker DMAs nothing; preempting a live co-writer's key over a lost
//! READ shard would fence its data plane).
//!
//! The fencing is persisted in `JobShardV1` at every bump (quirk §6 #11)
//! through the `Durable` seam, so a coordinator restart seeds the live word
//! from it and never re-issues a fencing a zombie still holds.

const std = @import("std");
const Allocator = std.mem.Allocator;
const log = @import("../../core/log.zig");
const jc = @import("../../meta/journal_core.zig");
const records = @import("records.zig");
const seams = @import("seams.zig");
const stats_mod = @import("stats.zig");

const stats = stats_mod.stats;
pub const ShardId = seams.ShardId;
pub const DestTuple = records.DestTuple;
pub const StableIdentity = records.StableIdentity;
pub const LeaseVerdict = seams.LeaseVerdict;

/// The sweeper's tick: observes shutdown promptly; the 25 ms floor keeps a
/// test-short TTL from spinning.
pub fn sweepTickNs(ttl_ms: u32) u64 {
    return @max(@as(u64, ttl_ms) * std.time.ns_per_ms / 4, 25 * std.time.ns_per_ms);
}

/// The most shards one coordinator leases at once: 64 destinations per
/// shard × 1024 = the 8-worker copy window × 128 remote members — far past
/// any fleet; a bounded table so the sweep allocates nothing.
pub const MAX_SHARDS: usize = 1024;

/// Where an expired lease's destinations go (the allocator's quarantine
/// under a fresh dead epoch) and how the cohort is released.
pub const QuarantineSink = struct {
    ctx: *anyopaque,
    vtable: *const VTable,

    pub const VTable = struct {
        /// Mint a dead epoch for one expiry's cohort.
        mintEpoch: *const fn (ctx: *anyopaque) u64,
        /// Admit `dest` to the cohort `epoch` (idempotent).
        admit: *const fn (ctx: *anyopaque, dest: DestTuple, epoch: u64) void,
        /// PR-preempt `holder`'s registration under the standing WERO on
        /// every data namespace; the count of namespaces the preempt
        /// landed on (0 on a detection-grade substrate or no WERO).
        preempt: *const fn (ctx: *anyopaque, holder: StableIdentity) u32,
        /// A landed preempt IS the drain proof: release the cohort.
        release: *const fn (ctx: *anyopaque, epoch: u64, namespaces: u32) void,
        /// Is `dest` quarantined NOW (the fresh-assignment tripwire)?
        isQuarantined: *const fn (ctx: *anyopaque, dest: DestTuple) bool,
    };
};

/// The persisted shard words (`JobShardV1`) at every bump.
pub const Durable = struct {
    ctx: *anyopaque,
    put: *const fn (ctx: *anyopaque, shard: records.JobShard) void,
};

const Entry = struct {
    id: ShardId,
    fencing: u64,
    holder: StableIdentity,
    /// Monotonic ns; 0 = unassigned.
    deadline_ns: u64,
    assigned: bool,
    done: bool,
    n_dest: u8,
    dests: [records.SHARD_DEST_MAX]DestTuple,
    /// The last expiry's cohort (0 = none).
    epoch: u64,

    fn destSlice(self: *const Entry) []const DestTuple {
        return self.dests[0..self.n_dest];
    }
};

pub const ShardTable = struct {
    lock: jc.SpinLock = .{},
    entries: [MAX_SHARDS]?Entry = @splat(null),
    n: usize = 0,
    ttl_ms: u32,
    sink: QuarantineSink,
    durable: ?Durable,
    /// Live-count instrument (the remote closure's in-flight term).
    in_flight: u64 = 0,

    pub fn init(ttl_ms: u32, sink: QuarantineSink, durable: ?Durable) ShardTable {
        return .{ .ttl_ms = ttl_ms, .sink = sink, .durable = durable };
    }

    pub fn lease(self: *ShardTable) seams.ShardLease {
        return .{ .ctx = self, .vtable = &vt };
    }

    const vt: seams.ShardLease.VTable = .{ .assign = &assignFn, .renew = &renewFn, .check = &checkFn, .complete = &completeFn, .expire = &expireFn, .retire = &retireFn };

    fn findLocked(self: *ShardTable, id: ShardId) ?*Entry {
        for (self.entries[0..self.n]) |*slot| if (slot.*) |*e| if (e.id.eql(id)) return e;
        return null;
    }

    fn persistLocked(self: *ShardTable, e: *const Entry, state: records.ShardState, now_ns: u64) void {
        const d = self.durable orelse return;
        d.put(d.ctx, .{ .job_id = e.id.job_id, .shard = e.id.shard, .state = state, .holder = e.holder, .lease_ttl_ms = self.ttl_ms, .shard_fencing = e.fencing, .updated_realtime_s = now_ns / std.time.ns_per_s, .dests = e.destSlice() });
    }

    /// Seed a shard's fencing from its durable record at adoption (the
    /// live word never regresses below what a zombie may hold).
    pub fn seed(self: *ShardTable, id: ShardId, fencing: u64) seams.ShardLease.Error!void {
        self.lock.lock();
        defer self.lock.unlock();
        if (self.findLocked(id)) |e| {
            e.fencing = @max(e.fencing, fencing);
            return;
        }
        const e = try self.newLocked(id);
        e.fencing = fencing;
    }

    fn newLocked(self: *ShardTable, id: ShardId) seams.ShardLease.Error!*Entry {
        var free: ?*?Entry = null;
        for (self.entries[0..self.n]) |*slot| if (slot.* == null) {
            free = slot;
            break;
        };
        if (free == null) {
            if (self.n >= MAX_SHARDS) return error.Full;
            free = &self.entries[self.n];
            self.n += 1;
        }
        free.?.* = .{ .id = id, .fencing = 0, .holder = .{ .node_token = 0, .mount_slot = 0 }, .deadline_ns = 0, .assigned = false, .done = false, .n_dest = 0, .dests = undefined, .epoch = 0 };
        return &free.?.*.?;
    }

    fn assignFn(ctx: *anyopaque, id: ShardId, holder: StableIdentity, dests: []const DestTuple, now_ns: u64) seams.ShardLease.Error!seams.ShardLease.Assigned {
        const self: *ShardTable = @ptrCast(@alignCast(ctx));
        std.debug.assert(dests.len <= records.SHARD_DEST_MAX);
        self.lock.lock();
        defer self.lock.unlock();
        for (dests) |d| if (self.sink.vtable.isQuarantined(self.sink.ctx, d)) {
            stats.job_quarantined_destination_allocated.fire("a fresh shard assignment carries a quarantined destination");
            return error.Full;
        };
        var e = self.findLocked(id);
        if (e == null) e = try self.newLocked(id);
        const en = e.?;
        if (en.assigned) {
            // Reassignment of a live lease (a departed session): the old
            // holder's window is quarantined first.
            self.expireLocked(en, now_ns);
        }
        if (en.done) return error.Full;
        en.assigned = true;
        en.holder = holder;
        en.deadline_ns = now_ns + @as(u64, self.ttl_ms) * std.time.ns_per_ms;
        en.n_dest = @intCast(dests.len);
        @memcpy(en.dests[0..dests.len], dests);
        self.in_flight += 1;
        stats.job_remote_shards.inc();
        self.persistLocked(en, .assigned, now_ns);
        return .{ .fencing = en.fencing, .lease_ttl_ms = self.ttl_ms };
    }

    fn verdictLocked(e: ?*Entry, fencing: u64) LeaseVerdict {
        const en = e orelse return .unknown;
        if (!en.assigned or en.done or en.fencing != fencing) return .stale;
        return .live;
    }

    fn renewFn(ctx: *anyopaque, id: ShardId, fencing: u64, now_ns: u64) LeaseVerdict {
        const self: *ShardTable = @ptrCast(@alignCast(ctx));
        self.lock.lock();
        defer self.lock.unlock();
        const e = self.findLocked(id);
        const v = verdictLocked(e, fencing);
        if (v == .live) e.?.deadline_ns = now_ns + @as(u64, self.ttl_ms) * std.time.ns_per_ms;
        return v;
    }

    fn checkFn(ctx: *anyopaque, id: ShardId, fencing: u64) LeaseVerdict {
        const self: *ShardTable = @ptrCast(@alignCast(ctx));
        self.lock.lock();
        defer self.lock.unlock();
        const v = verdictLocked(self.findLocked(id), fencing);
        if (v != .live) stats.job_remote_refused_stale.inc();
        return v;
    }

    fn completeFn(ctx: *anyopaque, id: ShardId, fencing: u64) LeaseVerdict {
        const self: *ShardTable = @ptrCast(@alignCast(ctx));
        self.lock.lock();
        defer self.lock.unlock();
        const e = self.findLocked(id);
        const v = verdictLocked(e, fencing);
        if (v != .live) {
            stats.job_remote_refused_stale.inc();
            return v;
        }
        e.?.done = true;
        e.?.assigned = false;
        self.in_flight -= 1;
        stats.job_remote_submissions.inc();
        self.persistLocked(e.?, .completed, 0);
        return .live;
    }

    /// The expiry arm under the lock: fencing +1, the cohort, the preempt,
    /// the requeue (the shard is unassigned again).
    fn expireLocked(self: *ShardTable, e: *Entry, now_ns: u64) void {
        e.fencing += 1;
        e.assigned = false;
        self.in_flight -= 1;
        stats.job_remote_lease_expiries.inc();
        stats.job_remote_reassignments.inc();
        if (e.n_dest > 0) {
            const epoch = self.sink.vtable.mintEpoch(self.sink.ctx);
            e.epoch = epoch;
            for (e.destSlice()) |d| self.sink.vtable.admit(self.sink.ctx, d, epoch);
            stats.job_remote_quarantined_destinations.add(e.n_dest);
            const landed = self.sink.vtable.preempt(self.sink.ctx, e.holder);
            if (landed > 0) {
                stats.job_remote_pr_preempts.inc();
                self.sink.vtable.release(self.sink.ctx, epoch, landed);
            } else {
                log.warn("job fabric: shard ({x}, {d}) expired at fencing {d}: {d} destination(s) stay quarantined under dead epoch {d} (no PR preempt landed — the deferred-reclaim class; the next fsck returns them)", .{ e.id.job_id, e.id.shard, e.fencing, e.n_dest, epoch });
            }
        }
        e.n_dest = 0;
        self.persistLocked(e, .reclaimed, now_ns);
    }

    fn expireFn(ctx: *anyopaque, id: ShardId, now_ns: u64) bool {
        const self: *ShardTable = @ptrCast(@alignCast(ctx));
        self.lock.lock();
        defer self.lock.unlock();
        const e = self.findLocked(id) orelse return false;
        if (!e.assigned or e.done) return false;
        self.expireLocked(e, now_ns);
        return true;
    }

    fn retireFn(ctx: *anyopaque, job_id: u64) void {
        const self: *ShardTable = @ptrCast(@alignCast(ctx));
        self.lock.lock();
        defer self.lock.unlock();
        for (self.entries[0..self.n]) |*slot| if (slot.*) |*e| if (e.id.job_id == job_id) {
            if (e.assigned and !e.done) self.in_flight -= 1;
            slot.* = null;
        };
        while (self.n > 0 and self.entries[self.n - 1] == null) self.n -= 1;
    }

    /// The sweeper's pass: expire every assigned lease whose deadline
    /// passed. Returns the count.
    pub fn sweep(self: *ShardTable, now_ns: u64) usize {
        self.lock.lock();
        defer self.lock.unlock();
        var n: usize = 0;
        for (self.entries[0..self.n]) |*slot| if (slot.*) |*e| {
            if (e.assigned and !e.done and now_ns >= e.deadline_ns) {
                self.expireLocked(e, now_ns);
                n += 1;
            }
        };
        return n;
    }

    /// Expire every lease `holder` holds NOW (a departed session).
    pub fn expireHolder(self: *ShardTable, holder: StableIdentity, now_ns: u64) usize {
        self.lock.lock();
        defer self.lock.unlock();
        var n: usize = 0;
        for (self.entries[0..self.n]) |*slot| if (slot.*) |*e| {
            if (e.assigned and !e.done and e.holder.node_token == holder.node_token and e.holder.mount_slot == holder.mount_slot) {
                self.expireLocked(e, now_ns);
                n += 1;
            }
        };
        return n;
    }

    /// Is `dest` inside the shard's CURRENT window (the proposal screen:
    /// every proposed tuple must be one the coordinator pre-allocated)?
    pub fn ownsDest(self: *ShardTable, id: ShardId, fencing: u64, dest: DestTuple) bool {
        self.lock.lock();
        defer self.lock.unlock();
        const e = self.findLocked(id) orelse return false;
        if (e.fencing != fencing or !e.assigned) return false;
        for (e.destSlice()) |d| if (d.eql(dest)) return true;
        return false;
    }

    /// Is the shard currently leased (assigned, not done)? A probe: no
    /// stat side effect.
    pub fn isAssigned(self: *ShardTable, id: ShardId) bool {
        self.lock.lock();
        defer self.lock.unlock();
        const e = self.findLocked(id) orelse return false;
        return e.assigned and !e.done;
    }

    pub fn inFlight(self: *ShardTable) u64 {
        self.lock.lock();
        defer self.lock.unlock();
        return self.in_flight;
    }

    pub fn fencingOf(self: *ShardTable, id: ShardId) ?u64 {
        self.lock.lock();
        defer self.lock.unlock();
        const e = self.findLocked(id) orelse return null;
        return e.fencing;
    }
};

// ---------------------------------------------------------------------------
// A RAM sink + durable (tests and the loopback coordinator).
// ---------------------------------------------------------------------------

pub const RamSink = struct {
    gpa: Allocator,
    epochs: u64 = 0,
    quarantined: std.AutoHashMapUnmanaged(u128, u64) = .empty,
    released: std.ArrayList(u64) = .empty,
    preempts: std.ArrayList(StableIdentity) = .empty,
    /// What `preempt` answers (0 = detection-grade).
    preempt_lands: u32 = 1,
    puts: std.ArrayList(records.JobShard) = .empty,

    pub fn init(gpa: Allocator) RamSink {
        return .{ .gpa = gpa };
    }

    pub fn deinit(self: *RamSink) void {
        self.quarantined.deinit(self.gpa);
        self.released.deinit(self.gpa);
        self.preempts.deinit(self.gpa);
        for (self.puts.items) |p| self.gpa.free(p.dests);
        self.puts.deinit(self.gpa);
    }

    pub fn sink(self: *RamSink) QuarantineSink {
        return .{ .ctx = self, .vtable = &sink_vt };
    }

    pub fn durable(self: *RamSink) Durable {
        return .{ .ctx = self, .put = &putFn };
    }

    fn key(d: DestTuple) u128 {
        return (@as(u128, d.vol_tag) << 64) | d.block_idx;
    }

    const sink_vt: QuarantineSink.VTable = .{ .mintEpoch = &mintEpoch, .admit = &admit, .preempt = &preempt, .release = &release, .isQuarantined = &isQuarantined };

    fn mintEpoch(ctx: *anyopaque) u64 {
        const self: *RamSink = @ptrCast(@alignCast(ctx));
        self.epochs += 1;
        return self.epochs;
    }
    fn admit(ctx: *anyopaque, d: DestTuple, epoch: u64) void {
        const self: *RamSink = @ptrCast(@alignCast(ctx));
        self.quarantined.put(self.gpa, key(d), epoch) catch @panic("RamSink (a test double): out of memory");
    }
    fn preempt(ctx: *anyopaque, holder: StableIdentity) u32 {
        const self: *RamSink = @ptrCast(@alignCast(ctx));
        self.preempts.append(self.gpa, holder) catch @panic("RamSink (a test double): out of memory");
        return self.preempt_lands;
    }
    fn release(ctx: *anyopaque, epoch: u64, _: u32) void {
        const self: *RamSink = @ptrCast(@alignCast(ctx));
        self.released.append(self.gpa, epoch) catch @panic("RamSink (a test double): out of memory");
        // A released cohort's blocks leave quarantine (the allocator's
        // `releaseQuarantine` returns them to the free list).
        var it = self.quarantined.iterator();
        var drop: std.ArrayList(u128) = .empty;
        defer drop.deinit(self.gpa);
        while (it.next()) |kv| if (kv.value_ptr.* == epoch) drop.append(self.gpa, kv.key_ptr.*) catch @panic("RamSink (a test double): out of memory");
        for (drop.items) |k| _ = self.quarantined.remove(k);
    }
    fn isQuarantined(ctx: *anyopaque, d: DestTuple) bool {
        const self: *RamSink = @ptrCast(@alignCast(ctx));
        return self.quarantined.contains(key(d));
    }
    fn putFn(ctx: *anyopaque, s: records.JobShard) void {
        const self: *RamSink = @ptrCast(@alignCast(ctx));
        var copy = s;
        copy.dests = self.gpa.dupe(DestTuple, s.dests) catch @panic("RamSink (a test double): out of memory");
        self.puts.append(self.gpa, copy) catch @panic("RamSink (a test double): out of memory");
    }

    pub fn lastPut(self: *const RamSink) ?records.JobShard {
        if (self.puts.items.len == 0) return null;
        return self.puts.items[self.puts.items.len - 1];
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

const h1: StableIdentity = .{ .node_token = 1, .mount_slot = 1 };
const h2: StableIdentity = .{ .node_token = 2, .mount_slot = 1 };

test "the lease/fencing matrix: assign starts at fencing 0; renew/check/complete are live under the current fencing; a heartbeat extends the deadline; an expiry bumps the fencing, quarantines the window, preempts the holder and releases the cohort; the late submit is refused stale; a reassignment carries fresh destinations" {
    const gpa = testing.allocator;
    var sink = RamSink.init(gpa);
    defer sink.deinit();
    var t = ShardTable.init(1000, sink.sink(), sink.durable());
    const l = t.lease();
    const id: ShardId = .{ .job_id = 9, .shard = 1 };
    const dests = [_]DestTuple{ .{ .vol_tag = 5, .block_idx = 10 }, .{ .vol_tag = 5, .block_idx = 11 } };
    const a = try l.assign(id, h1, &dests, 0);
    try testing.expectEqual(@as(u64, 0), a.fencing);
    try testing.expectEqual(@as(u32, 1000), a.lease_ttl_ms);
    try testing.expectEqual(@as(u64, 1), t.inFlight());
    try testing.expectEqual(records.ShardState.assigned, sink.lastPut().?.state);
    try testing.expect(t.ownsDest(id, 0, dests[1]) and !t.ownsDest(id, 0, .{ .vol_tag = 5, .block_idx = 12 }) and !t.ownsDest(id, 1, dests[0]));
    try testing.expectEqual(LeaseVerdict.live, l.check(id, 0));
    try testing.expectEqual(LeaseVerdict.stale, l.check(id, 1));
    try testing.expectEqual(LeaseVerdict.unknown, l.check(.{ .job_id = 9, .shard = 2 }, 0));
    // A heartbeat at 900 ms moves the deadline to 1900 ms: the sweep at
    // 1500 ms expires nothing.
    try testing.expectEqual(LeaseVerdict.live, l.renew(id, 0, 900 * std.time.ns_per_ms));
    try testing.expectEqual(@as(usize, 0), t.sweep(1500 * std.time.ns_per_ms));
    const exp0 = stats.job_remote_lease_expiries.load();
    try testing.expectEqual(@as(usize, 1), t.sweep(2000 * std.time.ns_per_ms));
    try testing.expectEqual(exp0 + 1, stats.job_remote_lease_expiries.load());
    try testing.expectEqual(@as(?u64, 1), t.fencingOf(id));
    try testing.expectEqual(@as(u64, 0), t.inFlight());
    try testing.expectEqual(records.ShardState.reclaimed, sink.lastPut().?.state);
    try testing.expectEqual(@as(u64, 1), sink.lastPut().?.shard_fencing);
    // The preempt landed ⇒ the cohort was released; the holder was preempted.
    try testing.expectEqual(@as(usize, 1), sink.preempts.items.len);
    try testing.expectEqual(h1, sink.preempts.items[0]);
    try testing.expectEqualSlices(u64, &.{1}, sink.released.items);
    try testing.expect(!sink.sink().vtable.isQuarantined(sink.sink().ctx, dests[0]));
    // The late submit under fencing 0 is refused stale; renew too.
    const stale0 = stats.job_remote_refused_stale.load();
    try testing.expectEqual(LeaseVerdict.stale, l.check(id, 0));
    try testing.expectEqual(LeaseVerdict.stale, l.complete(id, 0));
    try testing.expectEqual(LeaseVerdict.stale, l.renew(id, 0, 0));
    try testing.expectEqual(stale0 + 2, stats.job_remote_refused_stale.load());
    // Reassignment with fresh destinations at fencing 1; completes once.
    const fresh = [_]DestTuple{.{ .vol_tag = 5, .block_idx = 20 }};
    const b = try l.assign(id, h2, &fresh, 3 * std.time.ns_per_s);
    try testing.expectEqual(@as(u64, 1), b.fencing);
    try testing.expectEqual(LeaseVerdict.live, l.complete(id, 1));
    try testing.expectEqual(LeaseVerdict.stale, l.complete(id, 1));
    try testing.expectEqual(records.ShardState.completed, sink.lastPut().?.state);
    try testing.expect(!l.expire(id, 0));
    // Retire drops the state: unknown afterwards.
    l.retire(9);
    try testing.expectEqual(LeaseVerdict.unknown, l.check(id, 1));
    try testing.expectEqual(@as(usize, 0), t.n);
}

test "a detection-grade substrate (no preempt landed) keeps the cohort quarantined loud; the allocator never hands a quarantined idx to a fresh assignment (tripwire); a read shard (no destinations) expires with the fencing arm only — no epoch, no preempt" {
    const gpa = testing.allocator;
    var sink = RamSink.init(gpa);
    defer sink.deinit();
    sink.preempt_lands = 0;
    var t = ShardTable.init(100, sink.sink(), null);
    const l = t.lease();
    const id: ShardId = .{ .job_id = 1, .shard = 1 };
    const dests = [_]DestTuple{.{ .vol_tag = 1, .block_idx = 1 }};
    _ = try l.assign(id, h1, &dests, 0);
    try testing.expect(l.expire(id, 0));
    try testing.expect(sink.sink().vtable.isQuarantined(sink.sink().ctx, dests[0]));
    try testing.expectEqual(@as(usize, 0), sink.released.items.len);
    const trips = stats.job_quarantined_destination_allocated.load();
    log.testing.expectErrorLogs(1);
    stats.job_quarantined_destination_allocated.resetLogBudgetForTest();
    try testing.expectError(error.Full, l.assign(id, h2, &dests, 0));
    try log.testing.expectAllFired();
    try testing.expectEqual(trips + 1, stats.job_quarantined_destination_allocated.load());
    // A read shard.
    const rid: ShardId = .{ .job_id = 1, .shard = seams.INODE_PLANE_SHARD_BASE + 3 };
    _ = try l.assign(rid, h1, &.{}, 0);
    const epochs = sink.epochs;
    const preempts = sink.preempts.items.len;
    try testing.expectEqual(@as(usize, 1), t.sweep(std.time.ns_per_s));
    try testing.expectEqual(epochs, sink.epochs);
    try testing.expectEqual(preempts, sink.preempts.items.len);
    try testing.expectEqual(@as(?u64, 1), t.fencingOf(rid));
}

test "a departed session's held shards expire NOW; seeding from a durable fencing never regresses the live word; the sweep tick is max(TTL/4, 25 ms); the remote closure holds" {
    const gpa = testing.allocator;
    var sink = RamSink.init(gpa);
    defer sink.deinit();
    var t = ShardTable.init(30_000, sink.sink(), null);
    const l = t.lease();
    const d = [_]DestTuple{.{ .vol_tag = 1, .block_idx = 1 }};
    _ = try l.assign(.{ .job_id = 1, .shard = 1 }, h1, &d, 0);
    _ = try l.assign(.{ .job_id = 1, .shard = 2 }, h1, &.{}, 0);
    _ = try l.assign(.{ .job_id = 2, .shard = 1 }, h2, &.{}, 0);
    const before = stats_mod.stats.job_remote_shards.load() - stats_mod.stats.job_remote_submissions.load() - stats_mod.stats.job_remote_lease_expiries.load();
    try testing.expectEqual(@as(usize, 2), t.expireHolder(h1, 1));
    try testing.expectEqual(@as(u64, 1), t.inFlight());
    try testing.expect(stats_mod.remoteShardClosure(before - 2));
    try t.seed(.{ .job_id = 1, .shard = 1 }, 0);
    try testing.expectEqual(@as(?u64, 1), t.fencingOf(.{ .job_id = 1, .shard = 1 }));
    try t.seed(.{ .job_id = 7, .shard = 7 }, 42);
    try testing.expectEqual(@as(?u64, 42), t.fencingOf(.{ .job_id = 7, .shard = 7 }));
    const a = try l.assign(.{ .job_id = 7, .shard = 7 }, h1, &.{}, 0);
    try testing.expectEqual(@as(u64, 42), a.fencing);
    try testing.expectEqual(@as(u64, 7500 * std.time.ns_per_ms), sweepTickNs(30_000));
    try testing.expectEqual(@as(u64, 25 * std.time.ns_per_ms), sweepTickNs(10));
}
