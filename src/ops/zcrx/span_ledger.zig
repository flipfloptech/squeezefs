//! The span-grant ledger: per-slot refcounts + an MPSC intrusive free stack
//! (charter §6.3 "payload spans … holding chunk refcounts";
//! `docs/porting/ops_zcrx.md` §3.2). Portable; the lock-free core is
//! modelled exhaustively in `span_ledger_model.zig` with its three
//! weakenings.
//!
//! Protocol: `refs[i]` (0 = free); `next[i] = successor + 1` (0 = none);
//! `head = (pop_tag << 32) | (top + 1)` (0 = empty). `grant` pops the top
//! and sets `refs = 1` — ONE consumer per ledger (the queue driver), a
//! structural precondition the ledger cannot see (one driver per queue);
//! the single consumer also makes the pop ABA-free structurally, the tag is
//! a belt. `addRef` is any thread's clone edge (precondition: the caller
//! holds a ref — never resurrection). `release` is the Arc pattern:
//! `fetchSub(.release)`; the 0-crossing takes an acquire on the same word
//! (the release sequence of every other releaser) and pushes the slot free
//! with a release CAS, so the driver's re-grant (an acquire pop) happens
//! after every consumer's last read of the payload. Returns true to exactly
//! one releaser per grant epoch.
//!
//! In the zcrx leg a slot is the bookkeeping RECORD of a kernel-granted
//! CQE span (the refill rqe is posted when the record frees); in the venue
//! `RECV` leg it is the chunk the next recv lands in.

const std = @import("std");

pub const SpanLedger = struct {
    refs: []std.atomic.Value(u32),
    next: []std.atomic.Value(u32),
    head: std.atomic.Value(u64),

    /// Every slot free, chained `0 → 1 → … → slots-1`.
    pub fn init(gpa: std.mem.Allocator, slot_count: u32) std.mem.Allocator.Error!SpanLedger {
        std.debug.assert(slot_count >= 1);
        const refs = try gpa.alloc(std.atomic.Value(u32), slot_count);
        errdefer gpa.free(refs);
        const next = try gpa.alloc(std.atomic.Value(u32), slot_count);
        for (refs, next, 0..) |*r, *n, i| {
            r.* = .init(0);
            n.* = .init(if (i + 1 < slot_count) @intCast(i + 2) else 0);
        }
        return .{ .refs = refs, .next = next, .head = .init(1) };
    }

    pub fn deinit(self: *SpanLedger, gpa: std.mem.Allocator) void {
        gpa.free(self.refs);
        gpa.free(self.next);
        self.* = undefined;
    }

    pub fn slots(self: *const SpanLedger) u32 {
        return @intCast(self.refs.len);
    }

    /// Pop a free slot (the single consumer); null = exhausted.
    pub fn grant(self: *SpanLedger) ?u32 {
        // Acquire pairs with `pushFree`'s release CAS: every read of the
        // slot's payload by its last releaser (and, through its acquire on
        // the refcount, by every releaser) happens-before this pop.
        var head = self.head.load(.acquire);
        while (true) {
            if (head & 0xFFFF_FFFF == 0) return null;
            const top: u32 = @intCast((head & 0xFFFF_FFFF) - 1);
            const succ = self.next[top].load(.monotonic);
            const tag = (head >> 32) + 1;
            const new_head = (tag << 32) | succ;
            if (self.head.cmpxchgWeak(head, new_head, .acquire, .acquire)) |actual| {
                head = actual;
                continue;
            }
            self.refs[top].store(1, .monotonic);
            return top;
        }
    }

    /// A clone edge: the caller holds a ref on `slot`.
    pub fn addRef(self: *SpanLedger, slot: u32) void {
        const prev = self.refs[slot].fetchAdd(1, .monotonic);
        std.debug.assert(prev >= 1);
    }

    /// Drop one ref; true iff this call freed the slot.
    pub fn release(self: *SpanLedger, slot: u32) bool {
        // Release: this thread's reads of the payload precede the decrement.
        const prev = self.refs[slot].fetchSub(1, .release);
        std.debug.assert(prev >= 1);
        if (prev != 1) return false;
        // The 0-crossing: an acquire on the SAME word reads the tail of the
        // release sequence every other releaser's `fetchSub` joined, so their
        // payload reads happen-before the push below (the Arc fence).
        _ = self.refs[slot].load(.acquire);
        self.pushFree(slot);
        return true;
    }

    fn pushFree(self: *SpanLedger, slot: u32) void {
        var head = self.head.load(.monotonic);
        while (true) {
            self.next[slot].store(@intCast(head & 0xFFFF_FFFF), .monotonic);
            const new_head = (head & 0xFFFF_FFFF_0000_0000) | (@as(u64, slot) + 1);
            // Release pairs with `grant`'s acquire.
            if (self.head.cmpxchgWeak(head, new_head, .release, .monotonic)) |actual| {
                head = actual;
                continue;
            }
            return;
        }
    }

    pub fn refCount(self: *const SpanLedger, slot: u32) u32 {
        return self.refs[slot].load(.monotonic);
    }

    /// The free-stack length (a walk; quiescent instrument: `== slots` is
    /// the no-leak law).
    pub fn freeCount(self: *const SpanLedger) u32 {
        var n: u32 = 0;
        var cur: u32 = @intCast(self.head.load(.acquire) & 0xFFFF_FFFF);
        while (cur != 0) : (n += 1) cur = self.next[cur - 1].load(.monotonic);
        return n;
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

test "grant pops distinct slots until exhaustion; release returns them exactly once; addRef/release count; every slot comes back (free == total)" {
    var l = try SpanLedger.init(testing.allocator, 4);
    defer l.deinit(testing.allocator);
    try testing.expectEqual(@as(u32, 4), l.freeCount());
    var got: [4]u32 = undefined;
    for (&got) |*g| g.* = l.grant().?;
    try testing.expect(l.grant() == null);
    try testing.expectEqual(@as(u32, 0), l.freeCount());
    // Distinct.
    for (got, 0..) |a, i| for (got[i + 1 ..]) |b| try testing.expect(a != b);
    for (got) |g| try testing.expectEqual(@as(u32, 1), l.refCount(g));
    l.addRef(got[0]);
    l.addRef(got[0]);
    try testing.expectEqual(@as(u32, 3), l.refCount(got[0]));
    try testing.expect(!l.release(got[0]));
    try testing.expect(!l.release(got[0]));
    try testing.expect(l.release(got[0]));
    try testing.expectEqual(@as(u32, 1), l.freeCount());
    for (got[1..]) |g| try testing.expect(l.release(g));
    try testing.expectEqual(@as(u32, 4), l.freeCount());
    // Re-grant hands the most recently freed first (LIFO) and the tag advanced.
    const again = l.grant().?;
    try testing.expectEqual(got[3], again);
    try testing.expect(l.head.load(.monotonic) >> 32 == 5);
    try testing.expect(l.release(again));
}

test "4 threads contend releases of distinct slots against the single granter: every slot recycles, none is lost or duplicated" {
    const Slots = 64;
    const Rounds = 2000;
    var l = try SpanLedger.init(testing.allocator, Slots);
    defer l.deinit(testing.allocator);
    const Q = struct {
        ledger: *SpanLedger,
        inbox: [Slots]std.atomic.Value(u32) = @splat(.init(0)),
        fn consumer(self: *@This(), lane: usize) void {
            var done: u32 = 0;
            while (done < Rounds) {
                // Slots handed to this lane carry `lane + 1` in the inbox.
                for (&self.inbox, 0..) |*cell, s| {
                    if (cell.load(.acquire) == lane + 1) {
                        cell.store(0, .release);
                        _ = self.ledger.release(@intCast(s));
                        done += 1;
                    }
                }
            }
        }
    };
    var q: Q = .{ .ledger = &l };
    var threads: [4]std.Thread = undefined;
    for (&threads, 0..) |*t, i| t.* = try std.Thread.spawn(.{}, Q.consumer, .{ &q, i });
    var handed: u32 = 0;
    var lane: usize = 0;
    while (handed < 4 * Rounds) {
        const s = l.grant() orelse {
            std.atomic.spinLoopHint();
            continue;
        };
        // Hand the slot to a lane whose cell is empty (the ledger said the
        // slot is free, so its previous holder already cleared the cell).
        while (q.inbox[s].load(.acquire) != 0) std.atomic.spinLoopHint();
        q.inbox[s].store(@intCast(lane + 1), .release);
        lane = (lane + 1) % 4;
        handed += 1;
    }
    for (threads) |t| t.join();
    try testing.expectEqual(@as(u32, Slots), l.freeCount());
    for (0..Slots) |s| try testing.expectEqual(@as(u32, 0), l.refCount(@intCast(s)));
}
