//! THE COMMIT DOOR (charter §5.5 "The commit door: every commit takes a
//! token per slot before queueing (held to terminal outcome); `Releasing`
//! parks, a foreign lease refuses `SlotBusy` (EAGAIN, 'ship to the
//! holder'), unleased acquires first-touch with a parking admission";
//! `docs/porting/sym_slot_leases.md` §3): the ONE gate every commit passes
//! BEFORE it is queued (conveyor A0) and every slot release drains — the
//! rung-5 cutover gate (`routing/slot_gate.zig`, retired) folded in as the
//! `migrating` state of the same per-slot word (spec §3.6, decision 0002).
//! PORTABLE: the lease table's atomics + two `sync.Notify`s.
//!
//! The protocol is the house Dekker idiom over the table's hot cell:
//!
//!     committer: inflight[s] += 1 ; (SeqCst) ; load word[s] → own lease ⇒ token
//!     closer:    word[s] = releasing ; (SeqCst) ; load inflight[s] == 0 ⇒ drained
//!
//! both sides SeqCst (`slot_door_model.zig` carries the fence as an
//! explicit store-buffer drain and proves each weakening fails). The
//! verdict table (spec §3.1): ours `leased`/`offered` ⇒ the token; ours
//! `releasing` ⇒ PARK (register-recheck-await on `parkers`; the parker
//! resumes on the transfer's completion or its abort); another appender's
//! (`leased`/`offered`/`releasing`/`releasing{dead}`) ⇒ `SlotBusy`
//! (`errors.SlotMoved` = EAGAIN); an offer standing FOR this committer ⇒
//! the accept, through the same first-touch seam; `unleased` ⇒ the
//! first touch (the manager's in-process `AcquireSlot` with a PARKING
//! admission — the seam parks for ring space HOLDING NOTHING of the door;
//! `Deferred` ⇒ EAGAIN); `migrating` ⇒ park (rung 5's cutover park, its
//! own counter); an unarmed volume (no symmetric plane) has no lease
//! verdicts — only the cutover state parks.
//!
//! The holder-op note (spec §3.1 step 2) runs INSIDE the door before any
//! token, for the slots this mount holds: noted after a first touch, the
//! slot read IDLE with its commit's token out and the cadence released it
//! against the very commit the belt then refused (Issue 11). A refusal of a
//! foreign slot notes the REQUESTER's op on the dominance window — in one
//! process the refusal IS the requester's touch the served ship will carry
//! once the cross-owner intents land (wave C).
//!
//! Position (charter §2): the door sits between staging and queueing — a
//! committer holds its 4a guards and NO node lock, no ring admission. A
//! park here holds those guards; the closer side (a handover, a cutover)
//! takes no 4a guard and waits only for ADMITTED commits, which already
//! hold every guard they need — no cycle.

const std = @import("std");
const Io = std.Io;
const counters = @import("../../../core/counters.zig");
const clock = @import("../../../core/clock.zig");
const sync = @import("../../../sync/sync.zig");
const rstats = @import("../../routing/stats.zig");
const tx = @import("../../tx.zig");
const lt = @import("lease_table.zig");

const assert = std.debug.assert;

pub const LeaseTable = lt.LeaseTable;
pub const AppenderId = lt.AppenderId;

pub const stats = struct {
    /// A commit parked at the door on its own slot's `releasing`.
    pub var slot_door_parks: counters.ShardedCounter = .{};
    /// A commit refused `SlotBusy`: another appender leases the slot.
    pub var slot_door_refusals: counters.ShardedCounter = .{};
    /// A token holder admitted under `releasing` (the drain waits for it).
    pub var slot_door_draining_admits: counters.ShardedCounter = .{};
    /// A first touch at the door (`AcquireSlot` issued by the committer).
    pub var slot_acquires: counters.ShardedCounter = .{};
    /// Parked commits that resumed with a token (closure with refusals:
    /// `parked ≡ resumed + refused`).
    pub var slot_door_resumed: counters.ShardedCounter = .{};
    /// A parked commit that resumed into a refusal (the slot moved away).
    pub var slot_door_park_refused: counters.ShardedCounter = .{};
    /// A commit refused because the slot left this volume (the cutover
    /// flipped it while the commit parked, or it was never hosted here).
    pub var slot_door_moved: counters.ShardedCounter = .{};
};

/// Slots per transaction: a namespace op's ≤ 6 (rename's two parents, two
/// inodes and a whiteout mint, plus a mint) or the reclaim's destroy batch
/// of `RECLAIM_BATCH` = 64 corpses each in its own slot (`tx.DoorTokens`).
pub const MAX_SLOTS: usize = tx.DoorTokens.MAX_DOOR_SLOTS;

/// The tokens one commit holds: inline, no allocation (spec §9 Q3).
pub const Tokens = struct {
    slots: [MAX_SLOTS]u16 = undefined,
    len: u8 = 0,

    pub fn holds(self: *const Tokens, slot: u16) bool {
        for (self.slots[0..self.len]) |s| if (s == slot) return true;
        return false;
    }

    fn push(self: *Tokens, slot: u16) void {
        assert(self.len < MAX_SLOTS);
        self.slots[self.len] = slot;
        self.len += 1;
    }
};

pub const Error = error{
    /// Another appender leases the slot (`errors.SlotMoved`, EAGAIN).
    SlotBusy,
    /// The first touch was deferred (ring 0's window, the structural
    /// door's moving tail): EAGAIN, nothing written.
    Deferred,
    /// The slot is not hosted on this volume (a cutover moved it).
    SlotMoved,
    Io,
    Corrupt,
    OutOfMemory,
};

/// The first-touch seam: the manager's in-process `AcquireSlot(requester,
/// slot)` with the PARKING admission (an offer standing for the requester
/// is the accept inside the same verb). The seam parks for ring space
/// HOLDING NOTHING of the door and no verb mutex.
pub const FirstTouch = struct {
    ctx: *anyopaque,
    acquire: *const fn (ctx: *anyopaque, slot: u16, requester: AppenderId) Error!void,
};

/// Does this volume host the slot now (the route table's word)?
pub const Hosted = struct {
    ctx: *anyopaque,
    hosts: *const fn (ctx: *anyopaque, slot: u16) bool,
};

/// A foreign touch noted on the dominance window (the requester's op).
pub const ForeignTouch = struct {
    ctx: *anyopaque,
    note: *const fn (ctx: *anyopaque, slot: u16, holder: AppenderId, requester: AppenderId) void,
};

pub const Door = struct {
    table: *LeaseTable,
    io: Io,
    /// False on a single-writer volume: no lease verdicts, only the
    /// cutover state parks.
    armed: bool,
    hosted: ?Hosted = null,
    first_touch: ?FirstTouch = null,
    foreign_touch: ?ForeignTouch = null,
    /// `T_idle` for the holder-op buckets.
    t_idle_ns: u64,
    /// Parked committers (the `releasing` and `migrating` states).
    parkers: sync.Notify = .init,
    /// The closer waiting for a slot's tokens to drain.
    drained: sync.Notify = .init,

    pub const ParkWhy = enum { releasing, migrating };

    pub const Verdict = union(enum) {
        admitted,
        /// Own lease mid-handover, or the cutover's closed slot.
        park: ParkWhy,
        refuse: struct { holder: AppenderId, g: u32, dead: bool },
        /// Unleased on an armed volume: the committer acquires.
        first_touch,
        /// An offer stands for this committer: the accept.
        offered_to_me,
        /// Not hosted here.
        moved,
    };

    fn hostsSlot(self: *const Door, slot: u16) bool {
        const h = self.hosted orelse return true;
        return h.hosts(h.ctx, slot);
    }

    /// ONE attempt (spec §3.6 `tryEnter`): the RMW + the load; a passing
    /// verdict leaves the token IN, every other backs it out. "Ours" is the
    /// COMMITTING region's lease (`holder == me`): in one process a second
    /// region's slot is another appender's for every door decision (spec
    /// §6.2), which is what lets two regions hand a slot over in-process.
    pub fn tryEnter(self: *Door, slot: u16, me: AppenderId, now_ns: u64) Verdict {
        const v = self.table.enterToken(slot);
        switch (v.state) {
            .migrating => {
                _ = self.table.leaveToken(slot);
                return .{ .park = .migrating };
            },
            .unleased => {
                if (!self.armed) return .admitted;
                _ = self.table.leaveToken(slot);
                return if (self.hostsSlot(slot)) .first_touch else .moved;
            },
            .leased, .offered => {
                if (v.holder == me) return .admitted;
                _ = self.table.leaveToken(slot);
                if (v.state == .offered) {
                    if (self.table.offeredTo(slot, now_ns)) |to| if (to == me) return .offered_to_me;
                }
                return .{ .refuse = .{ .holder = v.holder, .g = v.g, .dead = false } };
            },
            .releasing => {
                _ = self.table.leaveToken(slot);
                if (v.holder == me and !v.dead) return .{ .park = .releasing };
                return .{ .refuse = .{ .holder = v.holder, .g = v.g, .dead = v.dead } };
            },
        }
    }

    /// The door for one commit: a token per distinct slot of `slots`,
    /// held in `out` to the terminal outcome. A refusal returns every
    /// token taken so far. `me` is the committing region's appender id.
    pub fn enter(self: *Door, slots: []const u16, me: AppenderId, out: *Tokens) Error!void {
        assert(out.len == 0);
        errdefer self.exit(out);
        for (slots) |slot| {
            if (out.holds(slot)) continue;
            try self.enterOne(slot, me, out);
        }
    }

    fn enterOne(self: *Door, slot: u16, me: AppenderId, out: *Tokens) Error!void {
        var parked: ?ParkWhy = null;
        while (true) {
            // Register BEFORE the try: a reopen between a failed try and
            // the park is never lost.
            var n = self.parkers.notified();
            _ = n.enable(self.io);
            const now = clock.monotonicNs();
            switch (self.tryEnter(slot, me, now)) {
                .admitted => {
                    n.cancel(self.io);
                    out.push(slot);
                    if (self.armed) self.table.noteHolderOp(slot, now, self.t_idle_ns);
                    if (parked != null) stats.slot_door_resumed.inc();
                    return;
                },
                .park => |why| {
                    if (parked == null) {
                        parked = why;
                        switch (why) {
                            .releasing => stats.slot_door_parks.inc(),
                            .migrating => rstats.stats.routing_gate_parks.inc(),
                        }
                    }
                    n.waitUncancelable(self.io);
                },
                .refuse => |r| {
                    n.cancel(self.io);
                    stats.slot_door_refusals.inc();
                    if (parked != null) stats.slot_door_park_refused.inc();
                    if (self.foreign_touch) |ft| ft.note(ft.ctx, slot, r.holder, me);
                    return error.SlotBusy;
                },
                .moved => {
                    n.cancel(self.io);
                    stats.slot_door_moved.inc();
                    if (parked != null) stats.slot_door_park_refused.inc();
                    return error.SlotMoved;
                },
                .first_touch, .offered_to_me => {
                    n.cancel(self.io);
                    const ft = self.first_touch orelse return error.SlotBusy;
                    stats.slot_acquires.inc();
                    // The seam answers when tree 0 names the lease (or the
                    // slot is somebody else's: the loop re-reads).
                    ft.acquire(ft.ctx, slot, me) catch |e| switch (e) {
                        error.SlotBusy => {},
                        else => return e,
                    };
                },
            }
        }
    }

    /// The terminal outcome: every token back; the last one out of a
    /// slot wakes the closer's drain.
    pub fn exit(self: *Door, tokens: *Tokens) void {
        for (tokens.slots[0..tokens.len]) |s| {
            if (self.table.leaveToken(s)) self.drained.notifyWaiters(self.io);
        }
        tokens.len = 0;
    }

    // ---- the closer's half (spec §3.2) ----

    /// `leased|offered → releasing` under `holder`: no new commit passes
    /// the door for the slot from here (the SeqCst store is the pair's).
    /// Answers `g`, null when the slot is not the holder's.
    pub fn beginRelease(self: *Door, slot: u16, holder: AppenderId, dead: bool) ?u32 {
        return self.table.beginRelease(slot, holder, dead);
    }

    pub const DrainOutcome = enum { drained, deadline };

    /// Every admitted commit on `slot` at its terminal outcome (apply
    /// included) — register-then-check on `drained`; bounded by the
    /// conveyor's own latency (the last token out wakes it) or the caller's
    /// deadline.
    pub fn drain(self: *Door, slot: u16, deadline_ns: u64) DrainOutcome {
        // The token holders inside: their applies proceed under
        // `releasing` and the drain waits for exactly them.
        stats.slot_door_draining_admits.add(self.table.inflightOf(slot));
        while (true) {
            var n = self.drained.notified();
            _ = n.enable(self.io);
            if (self.table.inflightOf(slot) == 0) {
                n.cancel(self.io);
                return .drained;
            }
            switch (n.waitUntilUncancelable(self.io, deadline_ns)) {
                .notified => {},
                .deadline => return if (self.table.inflightOf(slot) == 0) .drained else .deadline,
            }
        }
    }

    /// The abort: the slot accepts commits again, the parkers wake.
    pub fn endRelease(self: *Door, slot: u16) void {
        self.table.abortRelease(slot);
        self.parkers.notifyWaiters(self.io);
    }

    /// A handover or a release completed (the slot is unleased, or the
    /// requester's): the parkers re-read their verdict.
    pub fn wakeParkers(self: *Door) void {
        self.parkers.notifyWaiters(self.io);
    }

    // ---- the cutover fold (rung 5's gate) ----

    pub fn closeForCutover(self: *Door, slot: u16) void {
        self.table.closeForCutover(slot);
    }

    pub fn reopenCutover(self: *Door, slot: u16) void {
        self.table.reopenCutover(slot);
        self.parkers.notifyWaiters(self.io);
    }

    pub fn closedSlot(self: *const Door) ?u16 {
        return self.table.closedSlot();
    }

    pub fn inflightOf(self: *const Door, slot: u16) u32 {
        return self.table.inflightOf(slot);
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

const Harness = struct {
    table: *LeaseTable,
    door: Door,
    acquires: u32 = 0,
    foreign: u32 = 0,
    /// The first-touch seam grants to the requester (the manager's own
    /// first touch) unless `deferred`.
    deferred: bool = false,
    hosted_all: bool = true,

    fn init(gpa: std.mem.Allocator, armed: bool) !*Harness {
        const h = try gpa.create(Harness);
        h.* = .{ .table = try LeaseTable.init(gpa), .door = undefined };
        h.door = .{ .table = h.table, .io = testing.io, .armed = armed, .hosted = .{ .ctx = h, .hosts = hosts }, .first_touch = .{ .ctx = h, .acquire = acquire }, .foreign_touch = .{ .ctx = h, .note = note }, .t_idle_ns = 1 << 40 };
        return h;
    }

    fn deinit(self: *Harness, gpa: std.mem.Allocator) void {
        self.table.deinit();
        gpa.destroy(self);
    }

    fn hosts(ctx: *anyopaque, _: u16) bool {
        return @as(*Harness, @ptrCast(@alignCast(ctx))).hosted_all;
    }

    fn acquire(ctx: *anyopaque, slot: u16, requester: AppenderId) Error!void {
        const self: *Harness = @ptrCast(@alignCast(ctx));
        self.acquires += 1;
        if (self.deferred) return error.Deferred;
        _ = try self.table.acquire(slot, requester, false);
    }

    fn note(ctx: *anyopaque, _: u16, _: AppenderId, _: AppenderId) void {
        @as(*Harness, @ptrCast(@alignCast(ctx))).foreign += 1;
    }
};

test "the verdict table: own lease ⇒ token + a holder op; unleased ⇒ first touch then the token; a foreign lease ⇒ SlotBusy (counted, the window noted) with every earlier token returned; a deferred first touch ⇒ Deferred; not hosted ⇒ SlotMoved; an unarmed volume admits unleased slots" {
    const gpa = testing.allocator;
    const h = try Harness.init(gpa, true);
    defer h.deinit(gpa);
    _ = try h.table.acquire(3, 0, false);
    _ = try h.table.acquire(4, 7, false);
    var tok: Tokens = .{};
    try h.door.enter(&.{ 3, 3 }, 0, &tok);
    try testing.expectEqual(@as(u8, 1), tok.len);
    try testing.expectEqual(@as(u32, 1), h.door.inflightOf(3));
    try testing.expectEqual(@as(u64, 1), h.table.holderOps(3, clock.monotonicNs(), 1 << 40));
    h.door.exit(&tok);
    try testing.expectEqual(@as(u32, 0), h.door.inflightOf(3));
    // First touch of 5 by region 0: acquired, then the token.
    try h.door.enter(&.{5}, 0, &tok);
    try testing.expectEqual(@as(u32, 1), h.acquires);
    try testing.expect(h.table.view(5).leasedBy(0));
    try testing.expectEqual(@as(u8, 1), tok.len);
    h.door.exit(&tok);
    // Foreign slot 4 after own slot 3: refused, 3's token returned.
    const refusals0 = stats.slot_door_refusals.load();
    try testing.expectError(error.SlotBusy, h.door.enter(&.{ 3, 4 }, 0, &tok));
    try testing.expectEqual(@as(u8, 0), tok.len);
    try testing.expectEqual(@as(u32, 0), h.door.inflightOf(3));
    try testing.expectEqual(refusals0 + 1, stats.slot_door_refusals.load());
    try testing.expectEqual(@as(u32, 1), h.foreign);
    // A deferred first touch.
    h.deferred = true;
    try testing.expectError(error.Deferred, h.door.enter(&.{6}, 0, &tok));
    h.deferred = false;
    // Not hosted here.
    h.hosted_all = false;
    try testing.expectError(error.SlotMoved, h.door.enter(&.{6}, 0, &tok));
    h.hosted_all = true;
    // A dead holder's releasing slot refuses.
    _ = try h.table.acquire(8, 9, false);
    _ = h.table.beginRelease(8, 9, true).?;
    try testing.expectError(error.SlotBusy, h.door.enter(&.{8}, 0, &tok));
    // An offer standing for me is the accept through the seam.
    try testing.expectEqual(lt.OfferOutcome.offered, try h.table.offer(4, 7, 0, std.math.maxInt(u64)));
    _ = h.table.beginRelease(4, 7, false).?;
    _ = try h.table.release(4, 7, 1, .{}, 0);
    try testing.expect(h.table.view(4).state == .unleased);
    // The unarmed posture admits an unleased slot without a first touch.
    const u = try Harness.init(gpa, false);
    defer u.deinit(gpa);
    try u.door.enter(&.{ 1, 2 }, 0, &tok);
    try testing.expectEqual(@as(u8, 2), tok.len);
    try testing.expectEqual(@as(u32, 0), u.acquires);
    u.door.exit(&tok);
}

test "park and resume: a commit on an own releasing slot parks (counted once), the closer drains the admitted token, the abort wakes the parker into a token (resumed); a parker whose slot became foreign resumes into a refusal; parked ≡ resumed + refused" {
    const gpa = testing.allocator;
    const h = try Harness.init(gpa, true);
    defer h.deinit(gpa);
    _ = try h.table.acquire(3, 0, false);
    var admitted: Tokens = .{};
    try h.door.enter(&.{3}, 0, &admitted);
    try testing.expectEqual(@as(?u32, 1), h.door.beginRelease(3, 0, false));
    // A token holder's apply under releasing is admitted, counted.
    try testing.expectEqual(Door.DrainOutcome.deadline, h.door.drain(3, clock.monotonicNs() + 5 * std.time.ns_per_ms));
    const parks0 = stats.slot_door_parks.load();
    const resumed0 = stats.slot_door_resumed.load();
    const Parker = struct {
        fn run(d: *Door, out: *Tokens, res: *Error!void, done: *std.atomic.Value(bool)) void {
            res.* = d.enter(&.{3}, 0, out);
            done.store(true, .release);
        }
    };
    var tok: Tokens = .{};
    var res: Error!void = {};
    var done = std.atomic.Value(bool).init(false);
    const th = try std.Thread.spawn(.{}, Parker.run, .{ &h.door, &tok, &res, &done });
    while (stats.slot_door_parks.load() == parks0) std.Thread.yield() catch {};
    try testing.expect(!done.load(.acquire));
    h.door.exit(&admitted);
    try testing.expectEqual(Door.DrainOutcome.drained, h.door.drain(3, std.math.maxInt(u64)));
    try testing.expect(!done.load(.acquire));
    h.door.endRelease(3);
    th.join();
    try res;
    try testing.expectEqual(@as(u8, 1), tok.len);
    try testing.expectEqual(resumed0 + 1, stats.slot_door_resumed.load());
    h.door.exit(&tok);
    // The slot moves to a foreign holder while a parker waits: refused.
    _ = h.door.beginRelease(3, 0, false).?;
    const refused0 = stats.slot_door_park_refused.load();
    var tok2: Tokens = .{};
    var res2: Error!void = {};
    var done2 = std.atomic.Value(bool).init(false);
    const th2 = try std.Thread.spawn(.{}, Parker.run, .{ &h.door, &tok2, &res2, &done2 });
    while (stats.slot_door_parks.load() == parks0 + 1) std.Thread.yield() catch {};
    _ = try h.table.release(3, 0, 1, .{}, 0);
    _ = try h.table.acquire(3, 9, false);
    h.door.wakeParkers();
    th2.join();
    try testing.expectError(error.SlotBusy, res2);
    try testing.expectEqual(refused0 + 1, stats.slot_door_park_refused.load());
    try testing.expectEqual(stats.slot_door_parks.load() - parks0, (stats.slot_door_resumed.load() - resumed0) + (stats.slot_door_park_refused.load() - refused0));
}

test "the cutover fold: a closed slot parks every committer (routing_gate_parks), the closer drains the admitted ones and reopens; after a reopen that moved the slot off this volume the parker is SlotMoved" {
    const gpa = testing.allocator;
    const h = try Harness.init(gpa, false);
    defer h.deinit(gpa);
    var admitted: Tokens = .{};
    try h.door.enter(&.{7}, 0, &admitted);
    h.door.closeForCutover(7);
    try testing.expectEqual(@as(?u16, 7), h.door.closedSlot());
    const parks0 = rstats.stats.routing_gate_parks.load();
    const Parker = struct {
        fn run(d: *Door, out: *Tokens, res: *Error!void) void {
            res.* = d.enter(&.{7}, 0, out);
        }
    };
    var tok: Tokens = .{};
    var res: Error!void = {};
    const th = try std.Thread.spawn(.{}, Parker.run, .{ &h.door, &tok, &res });
    while (rstats.stats.routing_gate_parks.load() == parks0) std.Thread.yield() catch {};
    try testing.expectEqual(Door.DrainOutcome.deadline, h.door.drain(7, clock.monotonicNs() + 2 * std.time.ns_per_ms));
    h.door.exit(&admitted);
    try testing.expectEqual(Door.DrainOutcome.drained, h.door.drain(7, std.math.maxInt(u64)));
    // The slot flips away before the reopen: the parker is refused moved.
    h.hosted_all = false;
    h.door.armed = true;
    h.door.reopenCutover(7);
    th.join();
    try testing.expectError(error.SlotMoved, res);
    try testing.expectEqual(@as(?u16, null), h.door.closedSlot());
}
