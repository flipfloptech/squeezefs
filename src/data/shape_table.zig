//! The shape table: what a thread may know about an inode WITHOUT a KV
//! lookup — its claimed size and whether it is striped on a passthrough
//! set. One row per op-guard stripe keyed by the plane's effective ino (a
//! colliding ino evicts the row; a peek of the evicted ino answers null).
//! Written by the filesystem at every layout publish and size change (and
//! after a write's own size claim); read lock-free by the W1 hold gate at
//! delivery, the shared-mode admission probe, and — since
//! rung-10-write-iops — the shared-class write itself, whose prelude and
//! postlude run off the admission's snapshot: no layout lock, no KV read of
//! the kind or of the inode. That last reader makes the row LOAD-BEARING:
//! a torn snapshot (one ino's match with another ino's size or kind — the
//! three-word row of before could tear under a colliding publish, e.g. a
//! create's child landing on a row a sibling's write republishes) would
//! run the striped arm on an inline file or land an extending write with
//! no size claim. So the row is a seqlock whose odd state is the writer's
//! lock: a publisher takes the row by CAS (even → odd; a colliding
//! publisher spins the two stores out), writes, and releases (odd → even);
//! a reader takes the row only between two equal even reads of `seq`, else
//! retries, and past `peek_attempts` answers null (the exclusive arm —
//! never a spin on a row another ino keeps republishing). `ShapeRowModel`
//! proves the protocol; dropping the CAS (a plain entry bump) or the entry
//! bump altogether tears.
//!
//! Orderings: the CAS is `acq_rel` (its acquire half keeps the data stores
//! after it), the exit a `release` store (the data stores before it); the
//! reader's loads are all `acquire`, so none is hoisted above the one
//! before it — the first `seq` load stays first and the second last.

const std = @import("std");
const Atomic = std.atomic.Value;
const assert = std.debug.assert;

const explorer = @import("../model/explorer.zig");
const op_guard = @import("../fuse/handler/op_guard.zig");
const dlm = @import("../meta/dlm.zig");

/// The op-guard width: one row per stripe, so an ino's two tables keep one
/// shape and a collision here is a collision there.
pub const STRIPES: usize = op_guard.INODE_GUARD_STRIPES;

/// Retries a reader spends on a row whose publish is in flight or moved
/// under it before it answers null. A publish is two stores between its
/// bumps; two retries outlast any publisher that is not being rescheduled
/// mid-publish, and a null costs the one write an exclusive admission.
pub const peek_attempts: u32 = 3;

const STRIPED_PASSTHROUGH: u64 = 1;

/// `size << 1 | striped_passthrough`: one word, so the pair never tears
/// between themselves. A file size is below 2^54 (`u32` blocks × 4 MiB).
fn pack(size: u64, striped_passthrough: bool) u64 {
    assert(size < 1 << 63);
    return (size << 1) | @intFromBool(striped_passthrough);
}

pub const Snap = struct {
    size: u64,
    striped_passthrough: bool,

    fn unpack(w: u64) Snap {
        return .{ .size = w >> 1, .striped_passthrough = w & STRIPED_PASSTHROUGH != 0 };
    }
};

const Row = struct {
    /// Even between publishes, odd inside one (the writer's lock).
    seq: Atomic(u64) = .init(0),
    /// 0 = empty (never a FUSE nodeid nor an effective ino).
    ino: Atomic(u64) = .init(0),
    word: Atomic(u64) = .init(0),

    /// Take the row: even → odd. Spins out a colliding publisher's two
    /// stores (publishers of one ino are serialized by its op guard; two
    /// inos meet here once per 4096 at a publish).
    fn enter(r: *Row) void {
        while (true) {
            const s = r.seq.load(.monotonic);
            if (s & 1 == 0) {
                // acq_rel: the data stores that follow cannot move above.
                if (r.seq.cmpxchgWeak(s, s + 1, .acq_rel, .monotonic) == null) return;
            }
            std.atomic.spinLoopHint();
        }
    }

    /// Release the row: odd → even. release: the data stores land before
    /// the row reads even again.
    fn leave(r: *Row) void {
        const s = r.seq.load(.monotonic);
        assert(s & 1 != 0);
        r.seq.store(s + 1, .release);
    }
};

pub const ShapeTable = struct {
    rows: []Row,

    pub fn init(gpa: std.mem.Allocator) std.mem.Allocator.Error!ShapeTable {
        const rows = try gpa.alloc(Row, STRIPES);
        @memset(rows, .{});
        return .{ .rows = rows };
    }

    pub fn deinit(self: *ShapeTable, gpa: std.mem.Allocator) void {
        gpa.free(self.rows);
    }

    fn row(self: *ShapeTable, ino: u64) *Row {
        return &self.rows[@intCast(dlm.splitmix64(ino) & (STRIPES - 1))];
    }

    /// Publish `ino`'s shape.
    pub fn publish(self: *ShapeTable, ino: u64, size: u64, striped_passthrough: bool) void {
        const r = self.row(ino);
        r.enter();
        r.ino.store(ino, .monotonic);
        r.word.store(pack(size, striped_passthrough), .monotonic);
        r.leave();
    }

    /// Drop `ino`'s row (its forget); a row another ino took stays.
    pub fn forget(self: *ShapeTable, ino: u64) void {
        const r = self.row(ino);
        if (r.ino.load(.acquire) != ino) return;
        r.enter();
        if (r.ino.load(.monotonic) == ino) r.ino.store(0, .monotonic);
        r.leave();
    }

    /// The whole published snapshot of `ino`, or null: no row, another
    /// ino's, or a publish in flight across every attempt.
    pub fn peek(self: *ShapeTable, ino: u64) ?Snap {
        const r = self.row(ino);
        var attempt: u32 = 0;
        while (attempt < peek_attempts) : (attempt += 1) {
            const s1 = r.seq.load(.acquire);
            if (s1 & 1 != 0) continue;
            const got_ino = r.ino.load(.acquire);
            const w = r.word.load(.acquire);
            const s2 = r.seq.load(.acquire);
            if (s1 != s2) continue;
            if (got_ino != ino) return null;
            return Snap.unpack(w);
        }
        return null;
    }
};

// ---------------------------------------------------------------------------
// The model (charter §9): one row, two publishers of DIFFERENT inos (the
// colliding case: a create's child and a sibling's write) and one reader of
// the first; the reader must take only a snapshot one publisher wrote
// whole, or answer null.
// ---------------------------------------------------------------------------

const Weakening = struct {
    /// The entry is a plain bump, not a CAS: two publishers inside the row
    /// make `seq` even mid-flight and a reader straddles their stores.
    entry_without_cas: bool = false,
    /// No entry bump at all: a reader between two reads of the same even
    /// `seq` can straddle a data store.
    no_entry_bump: bool = false,
};

fn ShapeRowModel(comptime weakening: Weakening) type {
    return struct {
        const Self = @This();
        /// 0, 1 publish; 2 reads.
        pub const thread_count: usize = 3;
        const reader = 2;
        /// Publisher t writes ino `t + 1` with size `SIZE_STRIDE × (t + 1)`:
        /// any stride above `thread_count` keeps every publisher's size
        /// distinct from every ino, so a torn pair is never a coincidence.
        const SIZE_STRIDE = 10;
        /// The reader's program counters: the two `seq` reads (0 and 3), the
        /// data loads between (1, 2), the retry/give-up arm (5) and DONE (9);
        /// a publisher's: enter (0), two stores (1, 2), leave (3), DONE (9).
        const PC_SEQ1 = 0;
        const PC_LOAD_INO = 1;
        const PC_LOAD_SIZE = 2;
        const PC_SEQ2 = 3;
        const PC_RETRY = 5;
        const PC_DONE = 9;
        const PC_STORE_INO = 1;
        const PC_STORE_SIZE = 2;
        const PC_LEAVE = 3;
        fn inoOf(t: usize) u8 {
            return @intCast(t + 1);
        }
        fn sizeOf(t: usize) u8 {
            return @intCast(SIZE_STRIDE * (t + 1));
        }

        seq: u8 = 0,
        ino: u8 = 0,
        size: u8 = 0,
        ppc: [2]u8 = @splat(0),
        rpc: u8 = 0,
        r_s1: u8 = 0,
        r_ino: u8 = 0,
        r_size: u8 = 0,
        attempts: u8 = 0,
        /// The reader's answer: a snapshot of ino 1 (`taken`, with `answer`
        /// its size) or null.
        taken: bool = false,
        answer: u8 = 0,
        answered: bool = false,

        pub fn init() Self {
            return .{};
        }
        pub fn done(self: *const Self, t: usize) bool {
            return if (t == reader) self.rpc == PC_DONE else self.ppc[t] == PC_DONE;
        }
        pub fn step(self: *Self, t: usize) void {
            if (t == reader) return self.readerStep();
            switch (self.ppc[t]) {
                // enter: the CAS even → odd (a failed CAS is a no-op step —
                // the publisher spins)
                PC_SEQ1 => {
                    if (weakening.no_entry_bump) {
                        self.ppc[t] = PC_STORE_INO;
                    } else if (weakening.entry_without_cas) {
                        self.seq +%= 1;
                        self.ppc[t] = PC_STORE_INO;
                    } else if (self.seq & 1 == 0) {
                        self.seq += 1;
                        self.ppc[t] = PC_STORE_INO;
                    }
                },
                PC_STORE_INO => {
                    self.ino = inoOf(t);
                    self.ppc[t] = PC_STORE_SIZE;
                },
                PC_STORE_SIZE => {
                    self.size = sizeOf(t);
                    self.ppc[t] = PC_LEAVE;
                },
                // leave
                PC_LEAVE => {
                    self.seq +%= 1;
                    self.ppc[t] = PC_DONE;
                },
                else => unreachable,
            }
        }
        fn readerStep(self: *Self) void {
            switch (self.rpc) {
                PC_SEQ1 => {
                    self.r_s1 = self.seq;
                    self.attempts += 1;
                    self.rpc = if (self.r_s1 & 1 != 0) PC_RETRY else PC_LOAD_INO;
                },
                PC_LOAD_INO => {
                    self.r_ino = self.ino;
                    self.rpc = PC_LOAD_SIZE;
                },
                PC_LOAD_SIZE => {
                    self.r_size = self.size;
                    self.rpc = PC_SEQ2;
                },
                PC_SEQ2 => {
                    if (self.seq != self.r_s1) {
                        self.rpc = PC_RETRY;
                    } else {
                        self.taken = self.r_ino == inoOf(0);
                        self.answer = self.r_size;
                        self.answered = true;
                        self.rpc = PC_DONE;
                    }
                },
                // retry or give up
                PC_RETRY => {
                    if (self.attempts >= peek_attempts) {
                        self.answered = true;
                        self.rpc = PC_DONE;
                    } else self.rpc = PC_SEQ1;
                },
                else => unreachable,
            }
        }
        pub fn invariant(self: *const Self) !void {
            // A snapshot taken for ino 1 carries exactly publisher 0's size.
            if (self.answered and self.taken and self.answer != sizeOf(0)) return error.TornSnapshot;
        }
        pub fn final(self: *const Self) !void {
            if (!self.answered) return error.ReaderNeverAnswered;
            if (self.seq & 1 != 0) return error.PublishLeftOpen;
        }
    };
}

const testing = std.testing;

test "the shape row is a seqlock whose odd state is the writer's lock: a reader takes a whole publish or null; a plain entry bump or none tears under a colliding publish" {
    try explorer.expectHolds(ShapeRowModel(.{}), .{});
    try explorer.expectFails(ShapeRowModel(.{ .entry_without_cas = true }), .{}, error.InvariantViolated);
    try explorer.expectFails(ShapeRowModel(.{ .no_entry_bump = true }), .{}, error.InvariantViolated);
}

test "the shape table publishes whole and a torn row never matches a foreign ino" {
    var t = try ShapeTable.init(testing.allocator);
    defer t.deinit(testing.allocator);
    try testing.expect(t.peek(7) == null);
    t.publish(7, 4096, true);
    const s = t.peek(7).?;
    try testing.expectEqual(@as(u64, 4096), s.size);
    try testing.expect(s.striped_passthrough);
    try testing.expect(t.peek(8) == null);
    t.publish(7, 1 << 40, false);
    try testing.expectEqual(@as(u64, 1 << 40), t.peek(7).?.size);
    try testing.expect(!t.peek(7).?.striped_passthrough);
    t.forget(7);
    try testing.expect(t.peek(7) == null);
    // A forget of an ino that no longer holds the row leaves the holder.
    t.publish(9, 8192, true);
    t.forget(7);
    try testing.expectEqual(@as(u64, 8192), t.peek(9).?.size);
}

test "a peek inside a publish in flight answers null after its attempts, never a torn row" {
    var t = try ShapeTable.init(testing.allocator);
    defer t.deinit(testing.allocator);
    t.publish(7, 4096, true);
    const r = t.row(7);
    r.enter();
    r.word.store(pack(1 << 20, true), .monotonic);
    try testing.expect(t.peek(7) == null);
    r.leave();
    try testing.expectEqual(@as(u64, 1 << 20), t.peek(7).?.size);
}
