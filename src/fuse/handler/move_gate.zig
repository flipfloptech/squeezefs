//! THE MOVE-RETRY LAW at the FUSE and IPC boundary (decision 0003 §X.1): a
//! `Filesystem` over the filesystem whose every replying op that meets a
//! MOVE class (`errors.isMoveClass` — the cluster wire's typed retryable
//! answers: a slot moved or moving, a holder not yet resolved or
//! unreachable, a verb deferred) is RE-ISSUED by the system, never answered
//! EAGAIN: the op samples the mount's slot-move epoch before each attempt,
//! a refused attempt PARKS on it (woken by every local lease transition; a
//! remote one is seen at the next re-poll, doubling from one barrier to two
//! measured upper handovers, `slots/moves.zig`) and the op runs again from
//! the top — every home it touches re-resolved. Bounded by the fail-stop
//! lattice: past `deadline_ns` (`T_park_max` at the membership's T_owner)
//! the op answers the TERMINAL `HolderUnresolved` (EIO), loud; the mount's
//! close ends every park at once with the same answer.
//!
//! The re-issue is idempotent by construction: the op's `MoveAttempt`
//! rides `Ctx.move` across the attempts, and an op whose attempt left a
//! cross-owner intent OPEN resumes THAT intent (`kv_fs`; the op id is the
//! intent's id — a create answers its first attempt's ino, never EEXIST
//! against itself). An op that ends without resuming it (another error, the
//! deadline) abandons it to the roll-forward cadence. PORTABLE: the epoch is
//! a pointer, the wrapper a comptime-built vtable.

const std = @import("std");
const fs_mod = @import("filesystem.zig");
const counters = @import("../../core/counters.zig");
const clock = @import("../../core/clock.zig");
const log = @import("../../core/log.zig");
const errors = @import("../../core/errors.zig");
const moves = @import("../../meta/sym/slots/moves.zig");

const Filesystem = fs_mod.Filesystem;
const VTable = fs_mod.VTable;

pub const stats = struct {
    /// Ops re-issued by the system after a move-class refusal (one per
    /// re-issue; an op parked thrice counts three).
    pub var slot_move_reissues: counters.ShardedCounter = .{};
    /// Parks on the slot-move epoch (`moved` + `repoll` + closed ones).
    pub var slot_move_parks: counters.ShardedCounter = .{};
    /// Of those: woken by a local transition / ended at the re-poll bound.
    pub var slot_move_park_wakes: counters.ShardedCounter = .{};
    pub var slot_move_repolls: counters.ShardedCounter = .{};
    /// Ops whose move did not resolve within the lattice's deadline (or
    /// met the mount's close): answered `HolderUnresolved` (EIO), loud.
    pub var slot_move_unresolved: counters.ShardedCounter = .{};
    /// First refusal → the op's resolution, per op that met a move.
    pub var slot_move_ns: counters.ShardedLatencyHistogram = .{};
};

pub const MoveGate = struct {
    inner: Filesystem,
    moves: *moves.Moves,
    /// `moves.deadlineNs(T_park_max)`.
    deadline_ns: u64,

    pub fn filesystem(self: *MoveGate) Filesystem {
        return .{ .impl = self, .vtable = &vt };
    }

    const vt: VTable = build();

    fn build() VTable {
        var v: VTable = .{};
        const info = @typeInfo(VTable).@"struct";
        inline for (info.field_names, info.field_types) |name, FnPtr| {
            @field(v, name) = &Thunk(name, FnPtr).call;
        }
        return v;
    }

    fn Thunk(comptime name: []const u8, comptime FnPtr: type) type {
        const Fn = @typeInfo(FnPtr).pointer.child;
        const info = @typeInfo(Fn).@"fn";
        const Args = info.param_types[2].?;
        const R = info.return_type.?;
        // The no-reply ops (destroy, forget) and INIT pass: nothing a move
        // could refuse is answered there.
        const gated = @typeInfo(R) == .error_union and !std.mem.eql(u8, name, "init");
        return struct {
            fn call(impl: *anyopaque, c: *fs_mod.Ctx, args: Args) R {
                const self: *MoveGate = @ptrCast(@alignCast(impl));
                const inner = @field(self.inner.vtable, name);
                if (comptime !gated) return inner(self.inner.impl, c, args);
                var attempt: fs_mod.MoveAttempt = .{};
                const outer = c.move;
                c.move = &attempt;
                defer {
                    attempt.abandonOpen();
                    c.move = outer;
                }
                var first_refusal: u64 = 0;
                var repoll_ns: u64 = moves.REPOLL_MIN_NS;
                while (true) {
                    const seen = self.moves.observe();
                    const r = inner(self.inner.impl, c, args);
                    const e = if (r) |ok| {
                        if (first_refusal != 0) stats.slot_move_ns.record(clock.monotonicNs() - first_refusal);
                        return ok;
                    } else |err| err;
                    if (!errors.isMoveClass(e)) {
                        if (first_refusal != 0) stats.slot_move_ns.record(clock.monotonicNs() - first_refusal);
                        return e;
                    }
                    const now = clock.monotonicNs();
                    if (first_refusal == 0) first_refusal = now;
                    const deadline = first_refusal +| self.deadline_ns;
                    if (now >= deadline or self.moves.isClosed()) return giveUp(e, now - first_refusal);
                    stats.slot_move_parks.inc();
                    switch (self.moves.park(seen, @min(now +| repoll_ns, deadline))) {
                        .moved => stats.slot_move_park_wakes.inc(),
                        .repoll => {
                            stats.slot_move_repolls.inc();
                            repoll_ns = moves.nextRepollNs(repoll_ns);
                        },
                        .closed => return giveUp(e, clock.monotonicNs() - first_refusal),
                    }
                    stats.slot_move_reissues.inc();
                }
            }

            fn giveUp(e: anyerror, waited_ns: u64) R {
                stats.slot_move_unresolved.inc();
                stats.slot_move_ns.record(waited_ns);
                log.warn("move gate: {s} met a slot move that did not resolve in {d} ms (last answer {s}) — answering EIO (the fail-stop lattice's deadline, or the mount's close)", .{ name, waited_ns / std.time.ns_per_ms, @errorName(e) });
                return error.HolderUnresolved;
            }
        };
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;
const op_trace = @import("op_trace.zig");

/// An inner filesystem that refuses `refusals` times with `refusal`, then
/// answers; its moves bump the epoch on the Nth refusal (a local
/// transition) or not at all (a remote one: the re-poll finds it).
const Flaky = struct {
    moves: *moves.Moves,
    refusals: u32,
    refusal: fs_mod.Error = error.SlotMoved,
    calls: u32 = 0,
    bump_on_refusal: bool = true,
    /// How many attempts, from the first, leave an intent open (the
    /// resume's handle; the next attempt consumes it).
    leave_open: u32 = 0,
    resumed: u32 = 0,
    abandoned: u32 = 0,

    pub fn getattr(self: *Flaky, c: *fs_mod.Ctx, _: fs_mod.GetattrArgs) fs_mod.Error!fs_mod.Attr {
        self.calls += 1;
        if (c.move) |m| if (m.open) |_| {
            m.open = null;
            self.resumed += 1;
        };
        if (self.calls <= self.refusals) {
            if (self.calls <= self.leave_open) if (c.move) |m| {
                m.open = .{ .home = 1, .tx_id = 7, .abandon = &abandon, .ctx = self };
            };
            if (self.bump_on_refusal) self.moves.bump();
            return self.refusal;
        }
        return .{ .attr_ttl_ns = 0, .attr = std.mem.zeroes(@import("../abi.zig").fuse_attr) };
    }

    pub fn setattr(self: *Flaky, c: *fs_mod.Ctx, _: fs_mod.SetattrArgs) fs_mod.Error!fs_mod.Attr {
        self.calls += 1;
        if (c.move) |m| m.open = .{ .home = 1, .tx_id = 9, .abandon = &abandon, .ctx = self };
        return error.InputOutput;
    }

    fn abandon(ctx: *anyopaque, _: u64) void {
        @as(*Flaky, @ptrCast(@alignCast(ctx))).abandoned += 1;
    }
};

fn testCtx(tr: *op_trace.OpTrace) fs_mod.Ctx {
    return .{ .unique = 1, .uid = 0, .gid = 0, .pid = 0, .io = testing.io, .trace = tr, .wd_slot = null, .window = &.{} };
}

test "the move gate re-issues an op refused by a move class until it answers — parked on the epoch (a local transition wakes it), counted, never EAGAIN; a resume handle left open by an attempt is the re-issue's to consume; a non-move error passes through and abandons what was left open" {
    var m = moves.Moves.init(testing.io);
    var flaky: Flaky = .{ .moves = &m, .refusals = 3, .leave_open = 1 };
    var gate: MoveGate = .{ .inner = Filesystem.of(Flaky, &flaky), .moves = &m, .deadline_ns = std.math.maxInt(u64) };
    const fs = gate.filesystem();
    var tr = op_trace.OpTrace.begin(1, .other);
    defer tr.finish();
    var c = testCtx(&tr);
    const reissues0 = stats.slot_move_reissues.load();
    const parks0 = stats.slot_move_parks.load();
    _ = try fs.vtable.getattr(fs.impl, &c, .{ .ino = 5, .fh = null });
    try testing.expectEqual(@as(u32, 4), flaky.calls);
    try testing.expectEqual(reissues0 + 3, stats.slot_move_reissues.load());
    try testing.expectEqual(parks0 + 3, stats.slot_move_parks.load());
    try testing.expectEqual(@as(u32, 1), flaky.resumed);
    try testing.expectEqual(@as(u32, 0), flaky.abandoned);
    try testing.expect(c.move == null);
    // Every move class is retried; a terminal error is not.
    flaky.calls = 0;
    flaky.refusal = error.HolderUnreachable;
    flaky.leave_open = 0;
    _ = try fs.vtable.getattr(fs.impl, &c, .{ .ino = 5, .fh = null });
    try testing.expectEqual(@as(u32, 4), flaky.calls);
    flaky.calls = 0;
    try testing.expectError(error.InputOutput, fs.vtable.setattr(fs.impl, &c, .{ .ino = 5, .attr = .{} }));
    try testing.expectEqual(@as(u32, 1), flaky.calls);
    try testing.expectEqual(@as(u32, 1), flaky.abandoned);
    // A WouldBlock is not a move: the application's own answer.
    flaky.calls = 0;
    flaky.refusal = error.WouldBlock;
    try testing.expectError(error.WouldBlock, fs.vtable.getattr(fs.impl, &c, .{ .ino = 5, .fh = null }));
    try testing.expectEqual(@as(u32, 1), flaky.calls);
}

test "a remote move (no local wake) is found by the re-poll; past the lattice's deadline the op answers HolderUnresolved (EIO), counted, and abandons its open intent; the mount's close ends a park at once" {
    var m = moves.Moves.init(testing.io);
    var flaky: Flaky = .{ .moves = &m, .refusals = 2, .bump_on_refusal = false };
    var gate: MoveGate = .{ .inner = Filesystem.of(Flaky, &flaky), .moves = &m, .deadline_ns = std.math.maxInt(u64) };
    const fs = gate.filesystem();
    var tr = op_trace.OpTrace.begin(1, .other);
    defer tr.finish();
    var c = testCtx(&tr);
    const repolls0 = stats.slot_move_repolls.load();
    _ = try fs.vtable.getattr(fs.impl, &c, .{ .ino = 5, .fh = null });
    try testing.expectEqual(repolls0 + 2, stats.slot_move_repolls.load());
    // A holder that never resolves: the deadline answers EIO.
    flaky.calls = 0;
    flaky.refusals = std.math.maxInt(u32);
    flaky.leave_open = std.math.maxInt(u32);
    gate.deadline_ns = 3 * moves.REPOLL_MIN_NS;
    const unresolved0 = stats.slot_move_unresolved.load();
    try testing.expectError(error.HolderUnresolved, fs.vtable.getattr(fs.impl, &c, .{ .ino = 5, .fh = null }));
    try testing.expectEqual(unresolved0 + 1, stats.slot_move_unresolved.load());
    try testing.expectEqual(@as(u32, 1), flaky.abandoned);
    // The close.
    flaky.calls = 0;
    flaky.leave_open = 0;
    gate.deadline_ns = std.math.maxInt(u64);
    m.close();
    try testing.expectError(error.HolderUnresolved, fs.vtable.getattr(fs.impl, &c, .{ .ino = 5, .fh = null }));
    try testing.expectEqual(@as(u32, 1), flaky.calls);
}
