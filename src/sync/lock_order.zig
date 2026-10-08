//! The lock-order RAIL (charter §2 "Lock order (never invert)"): a debug-
//! only ledger of the lock LEVELS each execution context holds, checked at
//! every tracked acquire site. Acquiring a level BELOW the highest one the
//! context already holds is an inversion — the ABBA shape the charter
//! forbids — and trips `lock_order_inversions` (must stay 0; a tripwire,
//! never a panic: the holder is mid-op). The rail compiles to nothing
//! outside `debug` builds unless `force_enabled` is set, so no release hot path
//! pays for it; the gate's debug unit suite and the smoke are its venue.
//!
//! Levels tracked (the charter's numbering; the gaps are untracked levels):
//! (1) the per-inode FUSE op guard, (3) the per-(inode, block) flush lock,
//! (3.5) the per-inode block-map meta stripe, (3.7) the plane's per-ino
//! layout lock (declared by `data/plane.zig`: taken INSIDE (3.5) by every
//! block-map publish, so it sits between (3.5) and (4a)), (4a) the metadata
//! object locks. Same-level acquisitions are legal (a canonical 4a set, a
//! level-1 pair) — the classes order their own stripes.
//!
//! Contexts: fibers interleave on one lane thread and NEVER migrate
//! (`exec/fiber.zig`: `home` is fixed at spawn), so the ledger is per
//! thread with every entry tagged by the context that took it, and a check
//! compares only the acquiring context's entries. The executor installs
//! `current_context` (the running fiber); a plain thread is context 0.
//!
//! A 4a set travels into its transaction and is released by the conveyor
//! on ITS thread (`meta/tx.zig` `GuardSet`): the committer calls
//! `handedOff(.object)` when it submits — its frame no longer owns the
//! guards, the entry's hold does — and a release on a thread that never
//! acquired is a no-op. An abandon releases on the acquiring thread and
//! pops normally.

const std = @import("std");
const builtin = @import("builtin");
const counters = @import("../core/counters.zig");

/// The comptime flag: a release build may carry the rail for a soak.
pub const force_enabled = false;
pub const enabled = builtin.mode == .debug or force_enabled;

/// The charter's levels, ordered. The integer IS the order.
pub const Level = enum(u8) {
    /// (1) the per-inode FUSE op guard (`fuse/handler/op_guard.zig`).
    op_guard = 10,
    /// (3) the per-(inode, block) flush lock (`data/write_seams.BlockLocks`).
    block = 30,
    /// (3.5) the per-inode block-map meta stripe (`data/layout.MetaStripes`).
    meta_stripe = 35,
    /// (3.7) the plane's per-ino layout lock (`data/layout.LayoutLocks`).
    layout = 37,
    /// (4a) the metadata object locks `I{ino}` / `D{parent,name}` (`meta/dlm.zig`).
    object = 40,

    pub fn charterName(self: Level) []const u8 {
        return switch (self) {
            .op_guard => "(1) op guard",
            .block => "(3) block lock",
            .meta_stripe => "(3.5) meta stripe",
            .layout => "(3.7) layout lock",
            .object => "(4a) object lock",
        };
    }
};

/// `lock_order_inversions`: a tracked acquire below a held level (must stay 0).
pub var lock_order_inversions: counters.Tripwire = .init("lock_order_inversions");
/// `lock_order_ledger_overflows`: an acquire the ledger could not record
/// (more than `max_held` tracked locks held by one thread; must stay 0 —
/// the deepest legal hold is a level-1 pair + (3) + (3.5) + (3.7) + the
/// WIDE canonical 4a set of `2 × dlm.wide_requests` the striped `rmdir`
/// takes).
pub var lock_order_ledger_overflows: counters.Tripwire = .init("lock_order_ledger_overflows");

/// The ledger depth: the deepest legal hold above (`2 × 136 + 5`), with
/// headroom to the next power of two.
pub const max_held: usize = 512;

const Held = struct { level: Level, ctx: usize };

threadlocal var held: [max_held]Held = undefined;
threadlocal var depth: usize = 0;

/// The execution context an acquisition belongs to (a fiber pointer on a
/// lane thread; 0 on a plain thread). Installed once by the executor.
pub var current_context: *const fn () usize = &threadContext;

fn threadContext() usize {
    return 0;
}

/// The highest level this context holds, if any.
fn top(ctx: usize) ?Level {
    var best: ?Level = null;
    for (held[0..depth]) |h| {
        if (h.ctx != ctx) continue;
        if (best == null or @backingInt(h.level) > @backingInt(best.?)) best = h.level;
    }
    return best;
}

/// The current context is about to PARK for a lock at `level`: the check
/// (an attempt below a held level is the inversion — the wedge happens at
/// the attempt, so the ledger records before the park) and the record.
pub fn acquiring(level: Level) void {
    if (!enabled) return;
    const ctx = current_context();
    if (top(ctx)) |t| {
        if (@backingInt(level) < @backingInt(t)) lock_order_inversions.fire(level.charterName());
    }
    push(level, ctx);
}

/// A TRY acquire at `level` succeeded: recorded, not checked — a try
/// cannot wedge (the charter's one sanctioned out-of-order addition is
/// try-only), but a later park beneath it is checked against it.
pub fn acquiredTry(level: Level) void {
    if (!enabled) return;
    push(level, current_context());
}

fn push(level: Level, ctx: usize) void {
    if (depth == max_held) {
        lock_order_ledger_overflows.fire("the lock-order ledger is full");
        return;
    }
    held[depth] = .{ .level = level, .ctx = ctx };
    depth += 1;
}

/// A tracked lock at `level` was released by the current context: the
/// context's most recent entry at that level leaves the ledger. A release
/// by a context that never acquired (the conveyor dropping a 4a set) is a
/// no-op.
pub fn released(level: Level) void {
    if (!enabled) return;
    const ctx = current_context();
    var i = depth;
    while (i > 0) {
        i -= 1;
        if (held[i].ctx == ctx and held[i].level == level) {
            remove(i);
            return;
        }
    }
}

/// Ownership of every lock this context holds at `level` moved to another
/// owner (a 4a set submitted with its transaction): the entries leave the
/// ledger here; the foreign release is then a no-op.
pub fn handedOff(level: Level) void {
    if (!enabled) return;
    const ctx = current_context();
    var i = depth;
    while (i > 0) {
        i -= 1;
        if (held[i].ctx == ctx and held[i].level == level) remove(i);
    }
}

/// Whether the current context holds `level` (a product rail's predicate:
/// a head Put outside (3.7) is a bug the plane counts). True when the rail
/// is compiled out.
pub fn holds(level: Level) bool {
    if (!enabled) return true;
    const ctx = current_context();
    for (held[0..depth]) |h| if (h.ctx == ctx and h.level == level) return true;
    return false;
}

fn remove(i: usize) void {
    var j = i;
    while (j + 1 < depth) : (j += 1) held[j] = held[j + 1];
    depth -= 1;
}

/// The ledger depth of this thread (every context) — a test observable.
pub fn heldCountForTest() usize {
    std.debug.assert(builtin.is_test);
    return depth;
}

// ---------------------------------------------------------------------------
// Tests. Every body runs on its own thread so a sibling test's ledger
// (the rail is thread-local) never leaks in.
// ---------------------------------------------------------------------------

const testing = std.testing;
const log = @import("../core/log.zig");

fn onFreshThread(comptime body: fn () anyerror!void) !void {
    const Runner = struct {
        fn run(out: *anyerror!void) void {
            out.* = body();
        }
    };
    var out: anyerror!void = {};
    const t = try std.Thread.spawn(.{}, Runner.run, .{&out});
    t.join();
    try out;
}

fn ascendingIsSilent() !void {
    if (!enabled) return error.SkipZigTest;
    const inv0 = lock_order_inversions.load();
    acquiring(.op_guard);
    acquiring(.block);
    acquiring(.meta_stripe);
    acquiring(.layout);
    acquiring(.object);
    acquiring(.object); // the canonical set's second stripe: same level
    try testing.expect(holds(.layout));
    try testing.expectEqual(@as(usize, 6), heldCountForTest());
    released(.object);
    released(.object);
    released(.layout);
    released(.meta_stripe);
    released(.block);
    released(.op_guard);
    try testing.expectEqual(@as(usize, 0), heldCountForTest());
    try testing.expect(!holds(.layout));
    try testing.expectEqual(inv0, lock_order_inversions.load());
}

test "ascending acquisitions and same-level siblings are silent; the ledger balances" {
    try onFreshThread(ascendingIsSilent);
}

fn inversionTrips() !void {
    if (!enabled) return error.SkipZigTest;
    const inv0 = lock_order_inversions.load();
    lock_order_inversions.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    // Finding 1's shape: (3.7) held, then a park for (3.5).
    acquiring(.layout);
    acquiring(.meta_stripe);
    try log.testing.expectAllFired();
    try testing.expectEqual(inv0 + 1, lock_order_inversions.load());
    released(.meta_stripe);
    released(.layout);
    try testing.expectEqual(@as(usize, 0), heldCountForTest());
}

test "a lower level taken under a higher one trips lock_order_inversions ONCE (the (3.7) → (3.5) shape of finding 1)" {
    try onFreshThread(inversionTrips);
}

fn handOffAndForeignRelease() !void {
    if (!enabled) return error.SkipZigTest;
    const inv0 = lock_order_inversions.load();
    // A 4a set taken, submitted with its tx (handed off), then the
    // committer takes (3.7) for the sweep it drains after the ack: silent.
    acquiring(.object);
    acquiring(.object);
    handedOff(.object);
    try testing.expectEqual(@as(usize, 0), heldCountForTest());
    released(.object); // the conveyor's release on a thread that never took it
    acquiring(.layout);
    released(.layout);
    try testing.expectEqual(inv0, lock_order_inversions.load());
    try testing.expectEqual(@as(usize, 0), heldCountForTest());
}

test "a handed-off 4a set leaves the ledger; the foreign release is a no-op; a later (3.7) is silent" {
    try onFreshThread(handOffAndForeignRelease);
}

fn contextsAreSeparate() !void {
    if (!enabled) return error.SkipZigTest;
    const Ctx = struct {
        var id: usize = 1;
        fn current() usize {
            return id;
        }
    };
    const saved = current_context;
    defer current_context = saved;
    current_context = &Ctx.current;
    const inv0 = lock_order_inversions.load();
    // Fiber 1 holds (4a) and parks; fiber 2 on the same thread takes (3).
    Ctx.id = 1;
    acquiring(.object);
    Ctx.id = 2;
    acquiring(.block);
    try testing.expect(!holds(.object));
    released(.block);
    Ctx.id = 1;
    try testing.expect(holds(.object));
    released(.object);
    try testing.expectEqual(inv0, lock_order_inversions.load());
    try testing.expectEqual(@as(usize, 0), heldCountForTest());
}

test "two contexts interleaved on one thread never see each other's levels (fibers on a lane)" {
    try onFreshThread(contextsAreSeparate);
}

fn overflowIsCounted() !void {
    if (!enabled) return error.SkipZigTest;
    const of0 = lock_order_ledger_overflows.load();
    lock_order_ledger_overflows.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    for (0..max_held + 1) |_| acquiring(.object);
    try log.testing.expectAllFired();
    try testing.expectEqual(of0 + 1, lock_order_ledger_overflows.load());
    for (0..max_held) |_| released(.object);
    try testing.expectEqual(@as(usize, 0), heldCountForTest());
}

fn tryIsRecordedNotChecked() !void {
    if (!enabled) return error.SkipZigTest;
    const inv0 = lock_order_inversions.load();
    // A try-acquired (4a) beside a held (3.7): sanctioned, silent …
    acquiring(.layout);
    acquiredTry(.object);
    try testing.expectEqual(inv0, lock_order_inversions.load());
    released(.object);
    released(.layout);
    // … but a try-held (3.7) followed by a PARK for (3.5) is checked.
    lock_order_inversions.resetLogBudgetForTest();
    log.testing.expectErrorLogs(1);
    acquiredTry(.layout);
    acquiring(.meta_stripe);
    try log.testing.expectAllFired();
    try testing.expectEqual(inv0 + 1, lock_order_inversions.load());
    released(.meta_stripe);
    released(.layout);
    try testing.expectEqual(@as(usize, 0), heldCountForTest());
}

test "a try acquire is recorded but never checked; a park beneath a try-held level is" {
    try onFreshThread(tryIsRecordedNotChecked);
}

test "the ledger's overflow is counted, never a crash" {
    try onFreshThread(overflowIsCounted);
}
