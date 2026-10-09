//! The bounded MPSC submission ring of slot indices (charter §7; spec
//! `docs/porting/il_session.md` §5). Producers are app threads (untrusted);
//! the consumer is the ONE service thread that owns the session's drain —
//! a session is pinned to one thread for life, and the consumer cursor
//! (`Consumer.head`) lives in daemon-PRIVATE memory, deliberately not in
//! the mapping, so a hostile client cannot fake or fork consumption.
//!
//! The cells are Vyukov's: `seq == pos` empty and claimable at `pos`;
//! `seq == pos + 1` published; `seq == pos + capacity` freed for the next
//! lap. A fresh (zeroed) memfd is a BROKEN seeding — every cell but 0 reads
//! "previous lap still occupied" = permanently full — so the daemon calls
//! `seedForSharing` exactly once before the fd is shared (the fd pass IS
//! the synchronization point).
//!
//! Guarantee to the daemon: memory safety and bounded behaviour, never
//! semantic validity — all indexing masked, corrupted seqs degrade to
//! full/empty, whatever indices come out are validated against the
//! daemon-owned slot table (`layout.Mapping.slot`).
//!
//! `sync/mpmc.zig`'s `BoundedMpmc` is a Vyukov MPMC over OWNED cells; this
//! ring's cells are foreign, client-writable shm and the consumer is single,
//! so the view here is BORROWED (`View`) with the cursor beside it.

const std = @import("std");
const layout = @import("layout.zig");

const Atomic = std.atomic.Value;
const RingCell = layout.RingCell;

pub const MIN_RING_ENTRIES = layout.MIN_RING_ENTRIES;
pub const MAX_RING_ENTRIES = layout.MAX_RING_ENTRIES;

/// A borrowed view over the ring region of a mapping.
pub const View = struct {
    tail: *Atomic(u32),
    cells: []RingCell,
    mask: u32,

    pub fn init(tail: *Atomic(u32), cells: []RingCell) View {
        std.debug.assert(cells.len >= MIN_RING_ENTRIES and cells.len <= MAX_RING_ENTRIES and std.math.isPowerOfTwo(cells.len));
        return .{ .tail = tail, .cells = cells, .mask = @intCast(cells.len - 1) };
    }

    pub fn fromMapping(m: layout.Mapping) View {
        return init(m.ringTail(), m.ringCells());
    }

    pub fn capacity(v: View) u32 {
        return @intCast(v.cells.len);
    }

    /// The daemon's ONE seeding before the fd is shared: `cell[i].seq = i`,
    /// `tail = 0`. Plain stores — nobody else can see the memory yet.
    pub fn seedForSharing(v: View) void {
        for (v.cells, 0..) |*c, i| {
            c.seq.store(@intCast(i), .monotonic);
            c.value.store(0, .monotonic);
        }
        v.tail.store(0, .monotonic);
    }

    /// Any producer. `false` = FULL (the previous-lap occupant is not freed):
    /// client-visible backpressure — unreachable for an honest client with
    /// `slots ≤ ring_entries` (one ring entry per CLAIMED slot), so the
    /// client treats it as its own corrupt bookkeeping and poisons; the
    /// daemon is never blocked.
    pub fn push(v: View, value: u32) bool {
        var tail = v.tail.load(.monotonic);
        while (true) {
            const cell = &v.cells[tail & v.mask];
            // Acquire pairs with the consumer's freeing Release store.
            const seq = cell.seq.load(.acquire);
            const dif: i32 = @bitCast(seq -% tail);
            if (dif == 0) {
                if (v.tail.cmpxchgWeak(tail, tail +% 1, .monotonic, .monotonic)) |observed| {
                    tail = observed;
                    continue;
                }
                cell.value.store(value, .monotonic);
                // Publication rides the cell seq, never the tail: pairs with
                // `pop`'s Acquire load.
                cell.seq.store(tail +% 1, .release);
                return true;
            } else if (dif < 0) {
                return false;
            } else {
                // Another producer published past us: re-read the tail.
                tail = v.tail.load(.monotonic);
            }
        }
    }
};

/// The daemon-private consumer cursor.
pub const Consumer = struct {
    head: u32 = 0,

    /// The one consumer. `null` = not published (or a hostile scribble:
    /// observed as empty, never out of bounds).
    pub fn pop(c: *Consumer, v: View) ?u32 {
        const cell = &v.cells[c.head & v.mask];
        // Acquire pairs with the publishing Release store.
        const seq = cell.seq.load(.acquire);
        if (seq -% (c.head +% 1) != 0) return null;
        const value = cell.value.load(.monotonic);
        // Free the cell for the next lap: Release pairs with `push`'s
        // Acquire load of the seq.
        cell.seq.store(c.head +% v.capacity(), .release);
        c.head +%= 1;
        return value;
    }
};

// ---------------------------------------------------------------------------
// Tests (spec §8: the ring_core rows).
// ---------------------------------------------------------------------------

const testing = std.testing;

const Fixture = struct {
    tail: Atomic(u32) = .init(0),
    cells: [64]RingCell = @splat(.{}),

    fn view(f: *Fixture, cap: usize) View {
        return View.init(&f.tail, f.cells[0..cap]);
    }
};

test "seedForSharing revives zeroed cells: a fresh memfd ring is permanently full for every cell but 0; seeded, every cell accepts" {
    var f: Fixture = .{};
    const v = f.view(4);
    // Zeroed: cell 0 (seq 0 == pos 0) accepts once; cell 1 reads seq 0 at
    // pos 1 ⇒ "previous lap" ⇒ full.
    try testing.expect(v.push(10));
    try testing.expect(!v.push(11));
    v.seedForSharing();
    var i: u32 = 0;
    while (i < 4) : (i += 1) try testing.expect(v.push(i));
    try testing.expect(!v.push(99));
}

test "fifo single producer single consumer; a full ring refuses then reopens after a pop" {
    var f: Fixture = .{};
    const v = f.view(4);
    v.seedForSharing();
    var c: Consumer = .{};
    try testing.expectEqual(@as(?u32, null), c.pop(v));
    try testing.expect(v.push(1));
    try testing.expect(v.push(2));
    try testing.expect(v.push(3));
    try testing.expect(v.push(4));
    try testing.expect(!v.push(5));
    try testing.expectEqual(@as(?u32, 1), c.pop(v));
    try testing.expect(v.push(5));
    try testing.expect(!v.push(6));
    try testing.expectEqual(@as(?u32, 2), c.pop(v));
    try testing.expectEqual(@as(?u32, 3), c.pop(v));
    try testing.expectEqual(@as(?u32, 4), c.pop(v));
    try testing.expectEqual(@as(?u32, 5), c.pop(v));
    try testing.expectEqual(@as(?u32, null), c.pop(v));
}

test "many laps keep order and capacity exact (through the u32 tail wrap)" {
    var f: Fixture = .{};
    const v = f.view(8);
    v.seedForSharing();
    var c: Consumer = .{};
    // Start both cursors near the u32 wrap so the lap arithmetic crosses it.
    const start: u32 = std.math.maxInt(u32) - 13;
    f.tail.store(start, .monotonic);
    c.head = start;
    // The seeding at an arbitrary position: cell `p & mask` carries `p` for
    // the first `capacity` positions from `start`.
    var p: u32 = start;
    while (p -% start < 8) : (p +%= 1) v.cells[p & v.mask].seq.store(p, .monotonic);
    var next_push: u32 = 0;
    var next_pop: u32 = 0;
    var round: u32 = 0;
    while (round < 1000) : (round += 1) {
        var k: u32 = 0;
        while (k < 5) : (k += 1) {
            try testing.expect(v.push(next_push));
            next_push += 1;
        }
        k = 0;
        while (k < 5) : (k += 1) {
            try testing.expectEqual(@as(?u32, next_pop), c.pop(v));
            next_pop += 1;
        }
    }
    var k: u32 = 0;
    while (k < 8) : (k += 1) try testing.expect(v.push(next_push + k));
    try testing.expect(!v.push(0));
}

test "a hostile cell scribble reads as empty or full, never out of bounds" {
    var f: Fixture = .{};
    const v = f.view(4);
    v.seedForSharing();
    var c: Consumer = .{};
    f.cells[0].seq.store(0xdead_beef, .monotonic);
    try testing.expectEqual(@as(?u32, null), c.pop(v));
    // The producer at pos 0 sees a far-future seq: "published past us" ⇒
    // re-reads the tail (still 0) ⇒ loops; a far-past seq ⇒ full. Pin the
    // bounded outcomes with a past value.
    f.cells[0].seq.store(0xffff_0000, .monotonic);
    try testing.expect(!v.push(1));
}

test "multi-producer stress: 4 producers × 4096 through capacity 64, every value popped exactly once" {
    var f: Fixture = .{};
    const v = f.view(64);
    v.seedForSharing();
    const per: u32 = 4096;
    const Producer = struct {
        fn run(view: View, base: u32) void {
            var i: u32 = 0;
            while (i < per) {
                if (view.push(base + i)) i += 1 else std.atomic.spinLoopHint();
            }
        }
    };
    var threads: [4]std.Thread = undefined;
    for (&threads, 0..) |*t, p| t.* = try std.Thread.spawn(.{}, Producer.run, .{ v, @as(u32, @intCast(p)) * per });
    var seen = try testing.allocator.alloc(bool, 4 * per);
    defer testing.allocator.free(seen);
    @memset(seen, false);
    var c: Consumer = .{};
    var popped: u32 = 0;
    while (popped < 4 * per) {
        if (c.pop(v)) |x| {
            try testing.expect(!seen[x]);
            seen[x] = true;
            popped += 1;
        } else std.atomic.spinLoopHint();
    }
    for (threads) |t| t.join();
    for (seen) |s| try testing.expect(s);
    try testing.expectEqual(@as(?u32, null), c.pop(v));
}

test "capacity rails: MIN 2 (a 1-cell ring is structurally broken), MAX 2^16 (seq − pos stays inside i32)" {
    try testing.expectEqual(@as(u32, 2), MIN_RING_ENTRIES);
    try testing.expectEqual(@as(u32, 1 << 16), MAX_RING_ENTRIES);
    // The 1-cell shape: position 1's claim check (seq == 1) IS position 0's
    // publication (seq == 0 + 1) — shown on the arithmetic the ring uses.
    const pos1_claim: u32 = 1;
    const pos0_published: u32 = 0 + 1;
    try testing.expectEqual(pos1_claim, pos0_published);
}
