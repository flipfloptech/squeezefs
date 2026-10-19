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
//! ONE word per worker, on its own cache line: the phase rides the stamp's
//! low bit (a pass stamp is nanoseconds; the bit is below any instrument's
//! grain), so a pass costs one store at its top and one at its park, and
//! no two workers' stores share a line (eight did at 8 B per stamp: with
//! two rings per queue a line carried eight workers' per-pass stores).
//!
//! PORTABLE: plain atomics, indexed by the group (`worker.Group.index`).

const std = @import("std");
const geometry = @import("../geometry.zig");

/// A group drains at least one queue, so the queue rail times the rings
/// per queue bounds the groups.
pub const MAX_GROUPS: usize = geometry.MAX_QUEUES * geometry.MAX_RINGS_PER_QUEUE;

const Stamp = struct {
    /// `now_ns | phase`: 0 = no worker has passed at that index.
    word: std.atomic.Value(u64) align(std.atomic.cache_line) = .init(0),
};

pub var stamps: [MAX_GROUPS]Stamp = @splat(.{});

const in_pass_bit: u64 = 1;

/// The worker's loop, at the top of every pass. Monotonic: the census reads
/// the stamp alone, never a field it publishes.
pub fn stamp(group: u16, now_ns: u64) void {
    stamps[group].word.store(now_ns | in_pass_bit, .monotonic);
}

/// The worker is about to park in its ring (the pass is over until a CQE).
pub fn parked(group: u16) void {
    const w = &stamps[group].word;
    w.store(w.load(.monotonic) & ~in_pass_bit, .monotonic);
}

/// The stamp's time and phase for the census (0 = never passed).
pub fn read(group: u16) struct { at_ns: u64, in_pass: bool } {
    const w = stamps[group].word.load(.monotonic);
    return .{ .at_ns = w & ~in_pass_bit, .in_pass = w & in_pass_bit != 0 };
}

pub const Stalest = struct { group: u16, age_ns: u64 };

/// The worker held longest INSIDE a pass (null: every worker is parked, or
/// none ever passed) — the holder candidate.
pub fn stalest(now_ns: u64) ?Stalest {
    var out: ?Stalest = null;
    for (0..MAX_GROUPS) |g| {
        const r = read(@intCast(g));
        if (r.at_ns == 0 or !r.in_pass) continue;
        const age = now_ns -| r.at_ns;
        if (out == null or age > out.?.age_ns) out = .{ .group = @intCast(g), .age_ns = age };
    }
    return out;
}

/// A worker's teardown: a stopped worker is no holder.
pub fn clear(group: u16) void {
    stamps[group].word.store(0, .monotonic);
}

const testing = std.testing;

test "the stalest pass stamp names the held worker: the oldest stamp of a worker INSIDE a pass wins, a parked worker is no holder however old its stamp, a cleared worker is no holder, an empty table names none; the phase rides the stamp's low bit on its own line" {
    for (0..MAX_GROUPS) |g| clear(@intCast(g));
    defer for (0..MAX_GROUPS) |g| clear(@intCast(g));
    try testing.expectEqual(@as(usize, std.atomic.cache_line), @sizeOf(Stamp));
    try testing.expectEqual(@as(?Stalest, null), stalest(1_000));
    stamp(3, 900);
    stamp(7, 100);
    stamp(9, 950);
    try testing.expectEqual(Stalest{ .group = 7, .age_ns = 900 }, stalest(1_000).?);
    try testing.expect(read(7).in_pass);
    // The idle second ring of a queue: parked since its first pass, no holder.
    parked(7);
    try testing.expect(!read(7).in_pass);
    try testing.expectEqual(@as(u64, 100), read(7).at_ns);
    try testing.expectEqual(Stalest{ .group = 3, .age_ns = 100 }, stalest(1_000).?);
    clear(3);
    try testing.expectEqual(Stalest{ .group = 9, .age_ns = 50 }, stalest(1_000).?);
    parked(9);
    try testing.expectEqual(@as(?Stalest, null), stalest(1_000));
}
