//! Each FUSE drain worker's pass stamp: the monotonic time its loop last
//! began a pass and whether it is INSIDE that pass or parked, written by
//! the worker, read by the watchdog's wedge census (process-global atomics
//! only, readable no matter what is wedged).
//!
//! WHY: a worker's thread drives its handler lane and its ring (decision
//! 0003 §AB.3), so a worker held inside a pass — its thread parked raw by
//! something it runs inline — strands every fiber of the lane where it
//! stood and every request of its queues. The overdue op's station names
//! where the VICTIM stopped (the kamrui mdtest wedge: a create at
//! `handler_entry`, nothing else overdue); the stalest IN-PASS stamp names
//! the HOLDER — which worker stopped passing, and for how long. A parked
//! worker is no holder: an idle one owing nothing parks unbounded, so its
//! stamp's age is idle time (the 2026-10-19 census named group 0 at 30 s
//! for exactly that — the first ring of queue 0, idle since the mount).
//!
//! PORTABLE: plain atomics, indexed by the group (`worker.Group.index`).

const std = @import("std");
const geometry = @import("../geometry.zig");

/// A group drains at least one queue, so the queue rail bounds the groups.
pub const MAX_GROUPS: usize = geometry.MAX_QUEUES * geometry.MAX_RINGS_PER_QUEUE;

/// 0 = no worker has passed at that index.
pub var stamps: [MAX_GROUPS]std.atomic.Value(u64) = @splat(.init(0));
/// 1 = inside a pass since the stamp; 0 = parked in its ring (or never ran).
pub var in_pass: [MAX_GROUPS]std.atomic.Value(u8) = @splat(.init(0));

/// The worker's loop, at the top of every pass. Monotonic: the census reads
/// the stamp alone, never a field it publishes.
pub fn stamp(group: u16, now_ns: u64) void {
    stamps[group].store(now_ns, .monotonic);
    in_pass[group].store(1, .monotonic);
}

/// The worker is about to park in its ring (the pass is over until a CQE).
pub fn parked(group: u16) void {
    in_pass[group].store(0, .monotonic);
}

pub const Stalest = struct { group: u16, age_ns: u64 };

/// The worker held longest INSIDE a pass (null: every worker is parked, or
/// none ever passed) — the holder candidate.
pub fn stalest(now_ns: u64) ?Stalest {
    var out: ?Stalest = null;
    for (&stamps, 0..) |*s, g| {
        const at = s.load(.monotonic);
        if (at == 0 or in_pass[g].load(.monotonic) == 0) continue;
        const age = now_ns -| at;
        if (out == null or age > out.?.age_ns) out = .{ .group = @intCast(g), .age_ns = age };
    }
    return out;
}

/// A worker's teardown: a stopped worker is no holder.
pub fn clear(group: u16) void {
    stamps[group].store(0, .monotonic);
    in_pass[group].store(0, .monotonic);
}

const testing = std.testing;

test "the stalest pass stamp names the held worker: the oldest stamp of a worker INSIDE a pass wins, a parked worker is no holder however old its stamp, a cleared worker is no holder, an empty table names none" {
    for (&stamps, 0..) |*s, g| {
        s.store(0, .monotonic);
        in_pass[g].store(0, .monotonic);
    }
    defer for (&stamps, 0..) |*s, g| {
        s.store(0, .monotonic);
        in_pass[g].store(0, .monotonic);
    };
    try testing.expectEqual(@as(?Stalest, null), stalest(1_000));
    stamp(3, 900);
    stamp(7, 100);
    stamp(9, 950);
    try testing.expectEqual(Stalest{ .group = 7, .age_ns = 900 }, stalest(1_000).?);
    // The idle second ring of a queue: parked since its first pass, no holder.
    parked(7);
    try testing.expectEqual(Stalest{ .group = 3, .age_ns = 100 }, stalest(1_000).?);
    clear(3);
    try testing.expectEqual(Stalest{ .group = 9, .age_ns = 50 }, stalest(1_000).?);
    parked(9);
    try testing.expectEqual(@as(?Stalest, null), stalest(1_000));
}
