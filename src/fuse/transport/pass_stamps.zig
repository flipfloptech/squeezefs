//! Each FUSE drain worker's pass stamp: the monotonic time its loop last
//! began a pass, written once per pass, read by the watchdog's wedge census
//! (process-global atomics only, readable no matter what is wedged).
//!
//! WHY: a worker's thread drives its handler lane and its ring (decision
//! 0003 §AB.3), so a worker held inside a pass — its thread parked raw by
//! something it runs inline — strands every fiber of the lane where it
//! stood and every request of its queues. The overdue op's station names
//! where the VICTIM stopped (the kamrui mdtest wedge: a create at
//! `handler_entry`, nothing else overdue); the stalest stamp names the
//! HOLDER — which worker stopped passing, and for how long. An idle worker
//! passes at least once per ring-park backstop, so an age far past it is a
//! held worker.
//!
//! PORTABLE: plain atomics, indexed by the group (`worker.Group.index`).

const std = @import("std");
const geometry = @import("../geometry.zig");

/// A group drains at least one queue, so the queue rail bounds the groups.
pub const MAX_GROUPS: usize = geometry.MAX_QUEUES;

/// 0 = no worker has passed at that index.
pub var stamps: [MAX_GROUPS]std.atomic.Value(u64) = @splat(.init(0));

/// The worker's loop, at the top of every pass, on the COARSE monotonic clock
/// (the census reads ages in ms). Monotonic: the census reads the stamp
/// alone, never a field it publishes.
pub fn stamp(group: u16, now_ns: u64) void {
    stamps[group].store(now_ns, .monotonic);
}

pub const Stalest = struct { group: u16, age_ns: u64 };

/// The worker that passed longest ago (null: none ever passed).
pub fn stalest(now_ns: u64) ?Stalest {
    var out: ?Stalest = null;
    for (&stamps, 0..) |*s, g| {
        const at = s.load(.monotonic);
        if (at == 0) continue;
        const age = now_ns -| at;
        if (out == null or age > out.?.age_ns) out = .{ .group = @intCast(g), .age_ns = age };
    }
    return out;
}

/// A worker's teardown: a stopped worker is no holder.
pub fn clear(group: u16) void {
    stamps[group].store(0, .monotonic);
}

const testing = std.testing;

test "the stalest pass stamp names the held worker: the oldest live stamp wins, a cleared worker is no holder, an empty table names none" {
    for (&stamps) |*s| s.store(0, .monotonic);
    defer for (&stamps) |*s| s.store(0, .monotonic);
    try testing.expectEqual(@as(?Stalest, null), stalest(1_000));
    stamp(3, 900);
    stamp(7, 100);
    stamp(9, 950);
    try testing.expectEqual(Stalest{ .group = 7, .age_ns = 900 }, stalest(1_000).?);
    clear(7);
    try testing.expectEqual(Stalest{ .group = 3, .age_ns = 100 }, stalest(1_000).?);
}
