//! The result-carrying single-flight whole-block fetch — R1a (charter §6.3
//! "a result-carrying single-flight whole-block fetch (≤ 1 device fetch
//! per unique cold block; cohort waiters served from the primary's
//! result)"; `docs/porting/data_read_path.md` §4, §13 Q6, Q8). PORTABLE.
//!
//! One flight per block key. The first reader is the PRIMARY and fetches;
//! every reader that arrives while the flight is open is its COHORT and is
//! handed the primary's `FillResult` — a pin on the pool slot the bytes
//! landed in plus the serve-validity verdict and the incarnation word the
//! verdict was taken against, which EVERY waiter re-checks at its own
//! serve instant (the stored-snapshot revalidation, generic/209). A primary
//! that fails re-elects the front waiter (it fetches); a primary with no
//! waiters simply closes.
//!
//! THE GUARD RIDES THE PUBLISH (PERF-11): the flight stays open —
//! `inFlight(key)` true, late arrivals served from the stored result —
//! until the LAST hold is released, and the deferred NVMe publish takes a
//! hold. Without it a > 256 KiB block never enters the RAM LRU, so a
//! sub-block read landing between the serve and a late publish missed
//! every tier, found no flight, became a fresh primary and refetched the
//! whole block (the elbencho row-2 6.3× `get_obj` multiplier).
//!
//! No timer bounds a waiter (Q6: the reference's 60 s `MAX_WAIT` + 50 ms
//! slices answered `TimedOut` per op — a per-op timeout wrapper, banned):
//! a waiter parks on its own word under the 2 s ticked backstop (a lost
//! wake self-heals, counted) and a device that never answers is the
//! watchdog's overdue op. ONE cell per flight stores the terminal value
//! (Q8: no lag, no loop); a late subscriber reads it. Flights live in an
//! inline table of `max_flights` slots and allocate nothing per op.

const std = @import("std");
const Io = std.Io;
const Atomic = std.atomic.Value;
const assert = std.debug.assert;

const counters = @import("../core/counters.zig");
const tick = @import("../sync/tick.zig");
const rsc = @import("read_size_class.zig");
const fill_pool = @import("read_fill_pool.zig");

pub const BlockKey = rsc.BlockKey;

pub const stats = struct {
    /// Readers that joined an open flight and parked.
    pub var fill_cohort_joins: counters.ShardedCounter = .{};
    /// Waiters served from the primary's carried result.
    pub var singleflight_waiter_result_serves: counters.ShardedCounter = .{};
    /// Arrivals after the publish, served from the stored result (the
    /// guard-rides-the-publish window).
    pub var fill_cohort_late_result_serves: counters.ShardedCounter = .{};
    /// A failed primary's front waiter took over the fetch.
    pub var fill_cohort_reelections: counters.ShardedCounter = .{};
    /// Every slot was leading: the caller fetched un-deduplicated.
    pub var fill_cohort_table_full: counters.ShardedCounter = .{};
    /// Open flights (leading or published-with-holds).
    pub var fill_cohort_inflight: counters.Gauge = .{};
};

pub fn writeStatsJson(w: *std.Io.Writer) std.Io.Writer.Error!void {
    try w.print("{{\"fill_cohort_joins\":{d},\"singleflight_waiter_result_serves\":{d},\"fill_cohort_late_result_serves\":{d},\"fill_cohort_reelections\":{d},\"fill_cohort_table_full\":{d},\"fill_cohort_inflight\":{d}}}", .{
        stats.fill_cohort_joins.load(),
        stats.singleflight_waiter_result_serves.load(),
        stats.fill_cohort_late_result_serves.load(),
        stats.fill_cohort_reelections.load(),
        stats.fill_cohort_table_full.load(),
        stats.fill_cohort_inflight.load(),
    });
}

/// What a completed fill carries to its cohort.
pub const FillResult = struct {
    /// The pool slot the whole block landed in — the receiver holds ONE
    /// pin on it (taken under the table's lock) and releases it after its
    /// serve. Null = the primary fetched into an over-capacity backing the
    /// cohort cannot share (each waiter refetches: counted by the caller).
    slot: ?u32,
    len: u32,
    /// The primary's verdict after its own puts.
    serve_valid: bool,
    /// The incarnation word the verdict stands on (null = untracked or
    /// not publishable); a waiter re-checks `still(key, word)` at its
    /// serve instant.
    verdict_word: ?u64,
};

pub fn FlightTable(comptime max_flights: usize) type {
    return struct {
        const Self = @This();

        pub const capacity = max_flights;

        const State = enum(u8) { free, leading, published };

        const Slot = struct {
            key: BlockKey = undefined,
            state: State = .free,
            result: FillResult = undefined,
            /// Holds keeping the flight open: the primary's + every rider.
            holds: u32 = 0,
            head: ?*Waiter = null,
            tail: ?*Waiter = null,
        };

        const Outcome = enum(u8) { pending, result, elected };

        const Waiter = struct {
            next: ?*Waiter = null,
            prev: ?*Waiter = null,
            word: Atomic(u32) = .init(0),
            outcome: Outcome = .pending,
            result: FillResult = undefined,
        };

        interior: Io.Mutex = .init,
        slots: [max_flights]Slot = @splat(.{}),
        /// Open flights, written under `interior` at every open and close:
        /// `inFlight`'s miss answers from it without the lock and the
        /// `max_flights`-slot scan (the direct device leg asks per cold
        /// read — 4.4 % of the FUSE worker's cycles on rand-4k). A hint like
        /// the locked answer, which is stale the moment the lock drops.
        open: std.atomic.Value(u32) = .init(0),
        pool: *fill_pool.Pool,

        pub fn init(pool: *fill_pool.Pool) Self {
            return .{ .pool = pool };
        }

        /// The primary's handle: publish, fail, hold, release.
        pub const Guard = struct {
            table: *Self,
            idx: usize,

            /// Hand the result to every parked waiter (each gets its own
            /// pin) and store it for late arrivals (the flight keeps one
            /// reference of its own). The flight stays open until the
            /// last `release`.
            pub fn publish(self: Guard, io: Io, result: FillResult) void {
                const t = self.table;
                t.interior.lockUncancelable(io);
                defer t.interior.unlock(io);
                const s = &t.slots[self.idx];
                assert(s.state == .leading);
                s.state = .published;
                s.result = result;
                if (result.slot) |sl| t.pool.pin(sl);
                while (s.head) |w| {
                    unlink(s, w);
                    if (result.slot) |sl| t.pool.pin(sl);
                    w.result = result;
                    w.outcome = .result;
                    tick.wake(io, &w.word);
                }
            }

            /// The fetch failed: the front waiter is elected primary (the
            /// hold transfers to it); with no waiters the flight closes.
            pub fn fail(self: Guard, io: Io) void {
                const t = self.table;
                t.interior.lockUncancelable(io);
                defer t.interior.unlock(io);
                const s = &t.slots[self.idx];
                assert(s.state == .leading);
                if (s.head) |w| {
                    unlink(s, w);
                    w.outcome = .elected;
                    stats.fill_cohort_reelections.inc();
                    tick.wake(io, &w.word);
                    return;
                }
                assert(s.holds == 1);
                s.* = .{};
                _ = t.open.fetchSub(1, .monotonic);
                stats.fill_cohort_inflight.sub(1);
            }

            /// One more rider (the deferred publish); pairs with `release`.
            pub fn hold(self: Guard, io: Io) void {
                const t = self.table;
                t.interior.lockUncancelable(io);
                defer t.interior.unlock(io);
                t.slots[self.idx].holds += 1;
            }

            /// Drop one hold; the last one closes the flight and releases
            /// the stored result's reference. Legal only after `publish`.
            pub fn release(self: Guard, io: Io) void {
                const t = self.table;
                var unpin: ?u32 = null;
                {
                    t.interior.lockUncancelable(io);
                    defer t.interior.unlock(io);
                    const s = &t.slots[self.idx];
                    assert(s.state == .published and s.holds > 0);
                    s.holds -= 1;
                    if (s.holds != 0) return;
                    unpin = s.result.slot;
                    s.* = .{};
                    _ = t.open.fetchSub(1, .monotonic);
                    stats.fill_cohort_inflight.sub(1);
                }
                if (unpin) |sl| _ = t.pool.unpin(sl);
            }
        };

        pub const Join = union(enum) {
            /// This caller fetches.
            primary: Guard,
            /// Served from the primary's result (one pin on `slot`).
            result: FillResult,
            /// Every slot is leading: fetch un-deduplicated.
            table_full,
        };

        /// Lead or join the flight for `key`. A cohort member parks
        /// uncancelably (handlers are never cancelled, charter §9) under
        /// the ticked backstop until the primary publishes or fails.
        pub fn enter(self: *Self, io: Io, key: BlockKey) Join {
            var w: Waiter = .{};
            const idx: usize = blk: {
                self.interior.lockUncancelable(io);
                defer self.interior.unlock(io);
                if (self.findLocked(key)) |i| {
                    const s = &self.slots[i];
                    if (s.state == .published) {
                        if (s.result.slot) |sl| self.pool.pin(sl);
                        stats.fill_cohort_late_result_serves.inc();
                        return .{ .result = s.result };
                    }
                    push(s, &w);
                    stats.fill_cohort_joins.inc();
                    break :blk i;
                }
                const i = self.claimLocked(key) orelse {
                    stats.fill_cohort_table_full.inc();
                    return .table_full;
                };
                stats.fill_cohort_inflight.add(1);
                return .{ .primary = .{ .table = self, .idx = i } };
            };
            while (true) {
                const ticked = tick.parkUncancelable(io, &w.word, null) == .ticked;
                self.interior.lockUncancelable(io);
                if (w.outcome != .pending) tick.noteChannelRescue(ticked, &w.word);
                w.word.store(0, .monotonic);
                const outcome = w.outcome;
                self.interior.unlock(io);
                switch (outcome) {
                    .pending => continue,
                    .result => {
                        stats.singleflight_waiter_result_serves.inc();
                        return .{ .result = w.result };
                    },
                    .elected => return .{ .primary = .{ .table = self, .idx = idx } },
                }
            }
        }

        /// `key ∈ inflight_block_reads`: no flight open ⇒ no (one relaxed
        /// load — the count is a hint paired with no fence, as the locked
        /// scan's answer is); else one lock, no park.
        pub fn inFlight(self: *Self, io: Io, key: BlockKey) bool {
            if (self.open.load(.monotonic) == 0) return false;
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            return self.findLocked(key) != null;
        }

        /// Open flights (the deterministic observable).
        pub fn openCount(self: *Self, io: Io) usize {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            var n: usize = 0;
            for (&self.slots) |*s| n += @intFromBool(s.state != .free);
            return n;
        }

        /// Parked cohort members on `key`.
        pub fn cohort(self: *Self, io: Io, key: BlockKey) usize {
            self.interior.lockUncancelable(io);
            defer self.interior.unlock(io);
            const i = self.findLocked(key) orelse return 0;
            var n: usize = 0;
            var cur = self.slots[i].head;
            while (cur) |w| : (cur = w.next) n += 1;
            return n;
        }

        fn findLocked(self: *Self, key: BlockKey) ?usize {
            for (&self.slots, 0..) |*s, i| if (s.state != .free and s.key.eql(key)) return i;
            return null;
        }

        fn claimLocked(self: *Self, key: BlockKey) ?usize {
            for (&self.slots, 0..) |*s, i| if (s.state == .free) {
                s.* = .{ .key = key, .state = .leading, .holds = 1 };
                _ = self.open.fetchAdd(1, .monotonic);
                return i;
            };
            return null;
        }

        fn push(s: *Slot, w: *Waiter) void {
            w.next = null;
            w.prev = s.tail;
            if (s.tail) |t| t.next = w else s.head = w;
            s.tail = w;
        }

        fn unlink(s: *Slot, w: *Waiter) void {
            if (w.prev) |p| p.next = w.next else s.head = w.next;
            if (w.next) |n| n.prev = w.prev else s.tail = w.prev;
            w.next = null;
            w.prev = null;
        }
    };
}

// ---------------------------------------------------------------------------

const testing = std.testing;
const sync_testing = @import("../sync/testing.zig");

const T4 = FlightTable(4);

fn cohortBody(io: Io) !void {
    const pool = try fill_pool.Pool.initRam(testing.allocator, 4096, 8);
    defer pool.deinit();
    var table = T4.init(pool);
    const key = BlockKey.init(1, 0);
    const Ctx = struct {
        table: *T4,
        io: Io,
        key: BlockKey,
        served: Atomic(u32) = .init(0),
        fetched: Atomic(u32) = .init(0),
        go: Atomic(u32) = .init(0),
        fn run(c: *@This()) void {
            switch (c.table.enter(c.io, c.key)) {
                .primary => |g| {
                    _ = c.fetched.fetchAdd(1, .monotonic);
                    const slot = c.table.pool.acquire().?;
                    @memset(c.table.pool.bytes(slot), 0x42);
                    // Hold the cohort until the test says go (the device RTT).
                    while (c.go.load(.acquire) == 0) std.Thread.yield() catch {};
                    g.publish(c.io, .{ .slot = slot, .len = 4096, .serve_valid = true, .verdict_word = 3 });
                    // The primary's own reference.
                    _ = c.table.pool.unpin(slot);
                    g.release(c.io);
                },
                .result => |r| {
                    std.debug.assert(r.serve_valid and r.verdict_word.? == 3);
                    std.debug.assert(c.table.pool.bytes(r.slot.?)[0] == 0x42);
                    _ = c.served.fetchAdd(1, .monotonic);
                    _ = c.table.pool.unpin(r.slot.?);
                },
                .table_full => unreachable,
            }
        }
    };
    var ctx: Ctx = .{ .table = &table, .io = io, .key = key };
    var threads: [6]std.Thread = undefined;
    for (&threads) |*t| t.* = try std.Thread.spawn(.{}, Ctx.run, .{&ctx});
    // Wait until one primary leads and five are parked, then release.
    while (table.cohort(io, key) < 5) try std.Thread.yield();
    try testing.expect(table.inFlight(io, key));
    ctx.go.store(1, .release);
    for (threads) |t| t.join();
    try testing.expectEqual(@as(u32, 1), ctx.fetched.load(.monotonic));
    try testing.expectEqual(@as(u32, 5), ctx.served.load(.monotonic));
    try testing.expect(!table.inFlight(io, key));
    try testing.expectEqual(@as(usize, 8), pool.freeSlots());
    try testing.expectEqual(@as(usize, 0), table.openCount(io));
}

test "N concurrent readers of one cold block: ONE fetch, every waiter served from the carried result, every pin released" {
    try sync_testing.underBoth(cohortBody);
}

fn guardRidesPublishBody(io: Io) !void {
    const pool = try fill_pool.Pool.initRam(testing.allocator, 4096, 4);
    defer pool.deinit();
    var table = T4.init(pool);
    const key = BlockKey.init(2, 0);
    const g = switch (table.enter(io, key)) {
        .primary => |g| g,
        else => unreachable,
    };
    const slot = pool.acquire().?;
    // The deferred publish takes a hold BEFORE the result is published.
    g.hold(io);
    g.publish(io, .{ .slot = slot, .len = 4096, .serve_valid = true, .verdict_word = null });
    _ = pool.unpin(slot);
    // The flight's own reference keeps the slot alive.
    try testing.expectEqual(@as(u32, 1), pool.refCount(slot));
    // A late arrival in the publish window reads the stored result.
    const late_before = stats.fill_cohort_late_result_serves.load();
    switch (table.enter(io, key)) {
        .result => |r| {
            try testing.expectEqual(slot, r.slot.?);
            try testing.expectEqual(@as(u32, 2), pool.refCount(slot));
            _ = pool.unpin(r.slot.?);
        },
        else => return error.TestUnexpectedResult,
    }
    try testing.expectEqual(late_before + 1, stats.fill_cohort_late_result_serves.load());
    try testing.expect(table.inFlight(io, key));
    // The primary's release leaves the rider's hold.
    g.release(io);
    try testing.expect(table.inFlight(io, key));
    // The rider's release closes the flight and frees the slot.
    g.release(io);
    try testing.expect(!table.inFlight(io, key));
    try testing.expectEqual(@as(u32, 0), pool.refCount(slot));
    // A fresh arrival is a new primary.
    switch (table.enter(io, key)) {
        .primary => |g2| g2.fail(io),
        else => return error.TestUnexpectedResult,
    }
    try testing.expectEqual(@as(usize, 0), table.openCount(io));
}

test "the guard rides the publish: a late subscriber is served from the stored result until the last hold drops" {
    try sync_testing.underBoth(guardRidesPublishBody);
}

fn reelectionBody(io: Io) !void {
    const pool = try fill_pool.Pool.initRam(testing.allocator, 4096, 4);
    defer pool.deinit();
    var table = T4.init(pool);
    const key = BlockKey.init(3, 0);
    const Ctx = struct {
        table: *T4,
        io: Io,
        key: BlockKey,
        outcome: Atomic(u32) = .init(0),
        fn run(c: *@This()) void {
            switch (c.table.enter(c.io, c.key)) {
                .primary => |g| {
                    // Elected after the first primary failed: this fetch succeeds.
                    const slot = c.table.pool.acquire().?;
                    g.publish(c.io, .{ .slot = slot, .len = 4096, .serve_valid = true, .verdict_word = null });
                    _ = c.table.pool.unpin(slot);
                    g.release(c.io);
                    c.outcome.store(1, .release);
                },
                .result => c.outcome.store(2, .release),
                .table_full => c.outcome.store(3, .release),
            }
        }
    };
    const g = switch (table.enter(io, key)) {
        .primary => |g| g,
        else => unreachable,
    };
    var ctx: Ctx = .{ .table = &table, .io = io, .key = key };
    const t = try std.Thread.spawn(.{}, Ctx.run, .{&ctx});
    while (table.cohort(io, key) < 1) try std.Thread.yield();
    const re_before = stats.fill_cohort_reelections.load();
    g.fail(io);
    t.join();
    try testing.expectEqual(@as(u32, 1), ctx.outcome.load(.acquire));
    try testing.expectEqual(re_before + 1, stats.fill_cohort_reelections.load());
    try testing.expect(!table.inFlight(io, key));
    try testing.expectEqual(@as(usize, 4), pool.freeSlots());
}

test "a failing primary re-elects its front waiter, who fetches; a primary with no waiters closes the flight" {
    try sync_testing.underBoth(reelectionBody);
}

fn tableFullBody(io: Io) !void {
    const pool = try fill_pool.Pool.initRam(testing.allocator, 4096, 4);
    defer pool.deinit();
    var table = T4.init(pool);
    var guards: [4]T4.Guard = undefined;
    for (&guards, 0..) |*g, i| {
        g.* = switch (table.enter(io, .init(4, i * 4096))) {
            .primary => |p| p,
            else => unreachable,
        };
    }
    const full_before = stats.fill_cohort_table_full.load();
    try testing.expect(table.enter(io, .init(4, 99 * 4096)) == .table_full);
    try testing.expectEqual(full_before + 1, stats.fill_cohort_table_full.load());
    // The lock-free open count tracks the slots: a refused enter opens none.
    try testing.expectEqual(@as(u32, 4), table.open.load(.monotonic));
    try testing.expect(table.inFlight(io, .init(4, 2 * 4096)) and !table.inFlight(io, .init(4, 99 * 4096)));
    for (guards) |g| g.fail(io);
    try testing.expectEqual(@as(usize, 0), table.openCount(io));
    try testing.expectEqual(@as(u32, 0), table.open.load(.monotonic));
    try testing.expect(!table.inFlight(io, .init(4, 0)));
}

test "a full flight table is a counted refusal, never a park" {
    try sync_testing.underBoth(tableFullBody);
}
