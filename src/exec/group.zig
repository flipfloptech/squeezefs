//! `Io.Group` over fibers (charter §9 "a task set with genuine
//! cancel-on-drop" rides it through `sync/taskset.zig`; `docs/porting/exec.md`
//! §3.8) and the cancel request dispatch every cancellation shares.
//!
//! The group's `token` word packs `{canceled, have_awaiter, num_running}`
//! (std's `Io.Threaded` layout): zero ⇔ no members — what `Io.Group`
//! requires of the token. The join is ONE RMW on each side: the last member
//! `fetchSub`s and sees `have_awaiter`, or the awaiter `fetchOr`s
//! `have_awaiter` and sees `num_running == 0` — exactly one of them acts, so
//! no wake is lost and none is doubled (`group_model.zig`; the weakening is
//! the awaiter reading `num_running` and setting `have_awaiter` as two
//! steps). `state` holds the awaiter's cell (on the awaiter's frame).
//!
//! Members are found by their cancel word, never by a list: `cancelMembers`
//! walks every lane's fiber table and CASes `requested` into the fibers
//! whose word still names this group (`fiber.zig` `CancelWord`), so no list
//! lock and no allocation exist, and a slot re-spawned for another group
//! cannot be hit.
//!
//! Linux only (it drives fibers).

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const assert = std.debug.assert;
const futex = @import("../sync/futex.zig");
const fiber_mod = @import("fiber.zig");
const lane_mod = @import("lane.zig");
const stats = @import("stats.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("exec/group is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const Fiber = fiber_mod.Fiber;
pub const Lane = lane_mod.Lane;

pub const Token = packed struct(usize) {
    canceled: bool = false,
    have_awaiter: bool = false,
    num_running: Count = 0,

    pub const Count = @Int(.unsigned, @bitSizeOf(usize) - 2);
    pub const one_running: Token = .{ .num_running = 1 };
    pub const have_awaiter_bit: Token = .{ .have_awaiter = true };
    pub const canceled_bit: Token = .{ .canceled = true };

    pub fn running(n: usize) Token {
        return .{ .num_running = @intCast(n) };
    }
    pub fn toInt(self: Token) usize {
        return @bitCast(self);
    }
    pub fn fromInt(v: usize) Token {
        return @bitCast(v);
    }
};

/// The token as the atomic word it is.
pub fn statusPtr(g: *Io.Group) *std.atomic.Value(usize) {
    return @ptrCast(&g.token);
}

pub fn load(g: *Io.Group) Token {
    return Token.fromInt(statusPtr(g).load(.acquire));
}

pub fn isCanceled(g: *Io.Group) bool {
    return load(g).canceled;
}

/// A member is about to be spawned (before it can finish).
pub fn addMember(g: *Io.Group) void {
    _ = statusPtr(g).fetchAdd(Token.one_running.toInt(), .acq_rel);
}

/// A spawn that failed after `addMember`.
pub fn removeMember(g: *Io.Group) void {
    _ = statusPtr(g).fetchSub(Token.one_running.toInt(), .acq_rel);
}

/// The awaiter's cell, on its frame: a fiber (woken through the word) or a
/// raw thread (parked on `word`).
pub const AwaiterCell = struct {
    fiber: ?*Fiber,
    word: futex.Word = .init(0),
};

/// A member finished (called by its lane after the fiber switched out and
/// was recycled). The last one with an awaiter registered wakes it.
/// acq_rel pairs with `awaitInner`'s fetchOr: the cell pointer stored in
/// `state` before that RMW is visible here.
pub fn memberFinished(g: *Io.Group) void {
    const prev = Token.fromInt(statusPtr(g).fetchSub(Token.one_running.toInt(), .acq_rel));
    assert(prev.num_running >= 1);
    if (prev.num_running != 1 or !prev.have_awaiter) return;
    const cell: *AwaiterCell = @ptrFromInt(g.state);
    const f = cell.fiber;
    // After this store the awaiter may return and drop its frame: `cell`
    // is not touched again.
    cell.word.store(1, .release);
    if (f) |awaiter| {
        if (awaiter.state.wake()) awaiter.lane.enqueue(awaiter);
    } else {
        futex.wakeAll(&cell.word);
    }
}

/// `Group.await` (`cancelable`) and the wait half of `Group.cancel`.
/// `fiber` is the awaiter when it runs on a lane; null = a raw thread.
fn awaitInner(g: *Io.Group, fiber: ?*Fiber, lanes: []const *Lane) void {
    var cell: AwaiterCell = .{ .fiber = fiber };
    g.state = @intFromPtr(&cell);
    // acq_rel pairs with `memberFinished`'s fetchSub.
    const prev = Token.fromInt(statusPtr(g).fetchOr(Token.have_awaiter_bit.toInt(), .acq_rel));
    // ONE awaiter cell: a second concurrent awaiter displaced the first's
    // and the join wakes only the cell it finds (a caller bug; named).
    if (prev.have_awaiter and prev.num_running != 0) stats.group_second_awaiter.fire("a second concurrent awaiter of one Io.Group: the earlier awaiter's wake is lost");
    if (prev.num_running != 0) {
        if (fiber) |f| {
            f.awaited_group = g;
            if (f.changeAwaiting(.nothing, .group) and f.protection.check() == .unblocked) {
                // Cancelled before the park: propagate now; the members'
                // completion still ends the wait.
                cancelMembers(g, lanes);
            }
            while (cell.word.load(.acquire) == 0) f.park();
            _ = f.changeAwaiting(.group, .nothing);
            f.awaited_group = null;
        } else {
            while (cell.word.load(.acquire) == 0) futex.wait(&cell.word, 0);
        }
    }
    // Every member is gone: the token returns to the empty state `Io.Group` asserts.
    statusPtr(g).store(0, .release);
    g.state = 0;
}

/// `groupAwait`: a cancellation point for a fiber awaiter (a request that
/// landed during the wait propagated to the members and is reported here).
pub fn awaitGroup(g: *Io.Group, fiber: ?*Fiber, lanes: []const *Lane) Io.Cancelable!void {
    awaitInner(g, fiber, lanes);
    if (fiber) |f| try f.checkCancel();
}

/// `groupCancel`: mark the group (later spawns start cancelled), request
/// every member, wait for all of them.
pub fn cancelGroup(g: *Io.Group, fiber: ?*Fiber, lanes: []const *Lane) void {
    _ = statusPtr(g).fetchOr(Token.canceled_bit.toInt(), .acq_rel);
    cancelMembers(g, lanes);
    awaitInner(g, fiber, lanes);
}

/// Request cancellation on every fiber whose cancel word names `g`.
pub fn cancelMembers(g: *Io.Group, lanes: []const *Lane) void {
    for (lanes) |lane| {
        for (lane.fibers) |*f| requestCancel(f, g, lanes);
    }
}

/// THE cancel request (`exec.md` §3.8): set the bit (only if the fiber still
/// belongs to `group`, when given) and interrupt what it awaits — a plain
/// wake for a futex/timer park, an `ASYNC_CANCEL` by the owning lane for a
/// ring op, propagation for a group await; a future await, a blocking job
/// and a running fiber observe the bit at their next cancellation point.
pub fn requestCancel(f: *Fiber, group: ?*Io.Group, lanes: []const *Lane) void {
    const was = f.requestCancelWord(group) orelse return;
    switch (was.awaiting) {
        .nothing, .future, .blocking => {},
        .wakeable => if (f.state.wake()) f.lane.enqueue(f),
        .ring_op => f.lane.postCancelOp(f),
        .group => if (f.awaited_group) |ag| {
            _ = statusPtr(ag).fetchOr(Token.canceled_bit.toInt(), .acq_rel);
            cancelMembers(ag, lanes);
        },
    }
}

const testing = std.testing;

test "Token: zero is the empty group; the fields pack and unpack" {
    try testing.expectEqual(@as(usize, 0), (Token{}).toInt());
    const t = Token{ .canceled = true, .have_awaiter = true, .num_running = 3 };
    const back = Token.fromInt(t.toInt());
    try testing.expect(back.canceled and back.have_awaiter);
    try testing.expectEqual(@as(Token.Count, 3), back.num_running);
    var g: Io.Group = .init;
    try testing.expectEqual(@as(usize, 0), load(&g).num_running);
    addMember(&g);
    addMember(&g);
    try testing.expectEqual(@as(Token.Count, 2), load(&g).num_running);
    try testing.expect(g.token.load(.acquire) != null);
    removeMember(&g);
    removeMember(&g);
    try testing.expect(g.token.load(.acquire) == null);
}

test "the join over raw threads: the last member wakes a registered awaiter exactly once; an awaiter of an empty group returns at once" {
    var g: Io.Group = .init;
    try testing.expectEqual(@as(?*anyopaque, null), g.token.load(.acquire));
    awaitInner(&g, null, &.{});
    addMember(&g);
    addMember(&g);
    const Member = struct {
        fn run(grp: *Io.Group) void {
            memberFinished(grp);
        }
    };
    const a = try std.Thread.spawn(.{}, Member.run, .{&g});
    const b = try std.Thread.spawn(.{}, Member.run, .{&g});
    awaitInner(&g, null, &.{});
    a.join();
    b.join();
    try testing.expectEqual(@as(?*anyopaque, null), g.token.load(.acquire));
    try testing.expectEqual(@as(usize, 0), g.state);
}
