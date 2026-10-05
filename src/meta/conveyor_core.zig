//! The commit conveyor's handoff core (charter §5.2 "The commit conveyor",
//! §9 "the conveyor handoff"; survey `docs/porting/
//! kv_tree_cache_commit_survey.md` §3.1, §7.1): leader election, the FIFO
//! queue and the batch drain shared by the tx conveyor, the layout-merge
//! conveyor and the durability lane. The pass pipeline (admission, union
//! locks, journal write, fan-out) lives in the backend; nothing here touches
//! a device, an allocator or a timer. `conveyor_core_model.zig` explores the
//! enqueue / elect / drain / unlead interleavings with `model/explorer.zig`.
//!
//! ## The protocol
//!
//! Every committer runs the same two uninterruptible steps — no suspension
//! point between them, so a committer that disappears can never strand an
//! enqueued entry without an elected leader:
//!   1. `enqueue` (or `enqueueMany` for a group) its entry, FIFO;
//!   2. `tryLead` — on winning the CAS it spawns the detached pass task,
//!      then parks on its completion like every follower.
//!
//! The pass task loops: `drainBatch` a batch (bounded by the tx/byte caps,
//! NO timers — the batch is whatever is queued at drain time, jbd2's no-wait
//! shape); on an empty drain it calls `unleadAndRecheck`, which releases
//! leadership and THEN re-checks the queue, re-electing itself if an enqueue
//! raced the release. Release-then-recheck is the no-lost-wakeup theorem:
//!   - a committer that fails `tryLead` observed `leader == true` (seq_cst),
//!     i.e. it lost to a leader whose release had not yet happened;
//!   - its enqueue precedes its failed CAS in program order;
//!   - the leader's re-check follows its release in program order;
//!   - so release-after-CAS ⇒ recheck-after-enqueue ⇒ the entry is seen by
//!     the recheck or was already drained. Exactly one pass owns it.
//! Rechecking BEFORE releasing is the weakening the model proves loses the
//! wakeup; so is a non-seq_cst release (the store may sit in the store
//! buffer past the recheck — Dekker's shape).
//!
//! ## Groups
//!
//! `enqueueMany` pushes a whole set under ONE lock hold. Because the drain
//! takes the same lock, a drain never observes a partial group: it sees none
//! of it or all of it, in order — "N staged transactions = one apply pass"
//! is a property of the mechanism, not of arrival timing (given caps that
//! admit the group; the byte cap may still split it, progress-first).
//!
//! ## Invariants (the models)
//!
//!   1. leader uniqueness; 2. no lost wakeups; 3. FIFO; 4. budget
//!   conservation across committer drops; 5. guard lifetime ≥ staged-record
//!   lifetime (the entry co-owns the guard); 6. group atomicity.

const std = @import("std");
const counters = @import("../core/counters.zig");

/// The queue + leadership word. `T` is the backend's queue entry ({staged
/// records, shared guard set, completion channel} in production; small
/// tokens in the tests). Storage is a bounded ring the owner provides once:
/// hot ops allocate nothing, and a full ring is a typed refusal the
/// committer handles (never a silent drop).
pub fn ConveyorCore(comptime T: type) type {
    return struct {
        const Self = @This();

        /// One queued entry: the item and its budget bytes (the exact
        /// journal entry size — the drain's byte cap counts these).
        pub const Entry = struct {
            item: T,
            len: u64,
        };

        /// The drain caps: `max(64, cpus × 2)` txs / `max(256 KiB, ring/16)`
        /// bytes in the tx conveyor (derived by the backend, never here).
        pub const Caps = struct {
            max_txs: usize,
            max_bytes: u64,

            /// No cap: the durability lane drains everything queued.
            pub const unbounded: Caps = .{ .max_txs = std.math.maxInt(usize), .max_bytes = std.math.maxInt(u64) };
        };

        io: std.Io,
        /// `std.Io.Mutex` — Zig 0.17's futex mutex (the `std.Thread.Mutex`
        /// of earlier releases). The sanctioned metadata-transaction lock
        /// class (charter §12): held for O(1)/O(batch) queue ops only, never
        /// across a suspension point or any I/O.
        mutex: std.Io.Mutex = .init,
        /// The bounded ring: `slots[head ..][0..len]` modulo the capacity.
        slots: []Entry,
        head: usize = 0,
        len: usize = 0,
        /// Leadership word: true while exactly one pass task owns draining.
        leader: std.atomic.Value(bool) = .init(false),
        /// The queued-population gauge this core maintains (enqueue +n,
        /// drain −n), if any: maintained HERE so no fan-out path can leak
        /// it. The durability lane carries none — its population is gauged
        /// as windows in flight, which a drain does not end.
        gauge: ?*counters.Gauge,

        /// `slots` is the ring's storage for its whole life; its length is
        /// the capacity.
        pub fn init(io: std.Io, slots: []Entry, gauge: ?*counters.Gauge) Self {
            return .{ .io = io, .slots = slots, .gauge = gauge };
        }

        /// FIFO-enqueue one entry carrying `len` budget bytes.
        pub fn enqueue(self: *Self, item: T, len: u64) error{Full}!void {
            _ = try self.enqueueMany(&.{.{ .item = item, .len = len }});
        }

        /// FIFO-enqueue a GROUP under ONE lock hold: a concurrent drain sees
        /// none or all of it, in the given order. All-or-nothing on
        /// capacity too — a group that does not fit enqueues nothing.
        /// Returns the number enqueued.
        pub fn enqueueMany(self: *Self, entries: []const Entry) error{Full}!usize {
            {
                self.mutex.lockUncancelable(self.io);
                defer self.mutex.unlock(self.io);
                if (self.slots.len - self.len < entries.len) return error.Full;
                for (entries) |e| {
                    self.slots[(self.head + self.len) % self.slots.len] = e;
                    self.len += 1;
                }
            }
            if (entries.len > 0) if (self.gauge) |g| g.add(entries.len);
            return entries.len;
        }

        /// Attempt to become the leader. `true` ⇒ the caller MUST arrange a
        /// pass (spawn the detached task); `false` ⇒ a live leader is
        /// responsible for the queue (the no-lost-wakeup argument above).
        pub fn tryLead(self: *Self) bool {
            // seq_cst CAS paired with `unleadAndRecheck`'s seq_cst store:
            // the failed-CAS ↔ release ↔ recheck total order is exactly what
            // the theorem consumes. Election is once per pass, never hot.
            return self.leader.cmpxchgStrong(false, true, .seq_cst, .seq_cst) == null;
        }

        /// Drain the next batch into `out`, FIFO: at most `caps.max_txs`
        /// (and `out.len`) entries, at most `caps.max_bytes` summed budget —
        /// except the FIRST entry, which is always taken (progress: an entry
        /// larger than the byte cap must still commit; per-entry sizes are
        /// bounded upstream by the 128 KiB whole-entry cap). Only the leader
        /// calls this. Returns the filled prefix of `out`.
        pub fn drainBatch(self: *Self, caps: Caps, out: []T) []T {
            const max_txs = @min(@max(caps.max_txs, 1), out.len);
            var n: usize = 0;
            var bytes: u64 = 0;
            {
                self.mutex.lockUncancelable(self.io);
                defer self.mutex.unlock(self.io);
                while (n < max_txs and self.len > 0) {
                    const front = self.slots[self.head];
                    if (n > 0 and bytes + front.len > caps.max_bytes) break;
                    out[n] = front.item;
                    n += 1;
                    bytes += front.len;
                    self.head = (self.head + 1) % self.slots.len;
                    self.len -= 1;
                }
            }
            if (n > 0) if (self.gauge) |g| g.sub(n);
            return out[0..n];
        }

        /// Release leadership, THEN re-check the queue (the order IS the
        /// no-lost-wakeup theorem). `true` ⇒ the caller re-elected itself
        /// and must run another pass; `false` ⇒ it may exit (the queue is
        /// empty, or a racing enqueuer's `tryLead` won and owns the drain).
        pub fn unleadAndRecheck(self: *Self) bool {
            // seq_cst store paired with `tryLead`'s seq_cst CAS; a release
            // store here is the store-buffer weakening the model fails.
            self.leader.store(false, .seq_cst);
            if (self.pending() == 0) return false;
            return self.tryLead();
        }

        /// Entries currently queued (stats gauge + the conformance suite's
        /// enqueue-sequencing probe).
        pub fn pending(self: *Self) usize {
            self.mutex.lockUncancelable(self.io);
            defer self.mutex.unlock(self.io);
            return self.len;
        }
    };
}

// ---------------------------------------------------------------------------

const testing = std.testing;
const Core = ConveyorCore(u32);
const no_byte_cap = std.math.maxInt(u64);

test "drain_is_fifo_and_cap_bounded" {
    var slots: [16]Core.Entry = undefined;
    var c: Core = .init(testing.io, &slots, null);
    var i: u32 = 0;
    while (i < 10) : (i += 1) try c.enqueue(i, 10);
    try testing.expectEqual(@as(usize, 10), c.pending());

    var out: [16]u32 = undefined;
    // tx cap.
    try testing.expectEqualSlices(u32, &.{ 0, 1, 2, 3 }, c.drainBatch(.{ .max_txs = 4, .max_bytes = no_byte_cap }, &out));
    // byte cap: 25 bytes admits two 10-byte entries.
    try testing.expectEqualSlices(u32, &.{ 4, 5 }, c.drainBatch(.{ .max_txs = 64, .max_bytes = 25 }, &out));
    // first-entry progress: a cap below one entry still drains one.
    try testing.expectEqualSlices(u32, &.{6}, c.drainBatch(.{ .max_txs = 8, .max_bytes = 1 }, &out));
    // a zero tx cap is still one entry of progress.
    try testing.expectEqualSlices(u32, &.{7}, c.drainBatch(.{ .max_txs = 0, .max_bytes = no_byte_cap }, &out));
    // the out buffer bounds the batch too.
    try testing.expectEqualSlices(u32, &.{8}, c.drainBatch(.unbounded, out[0..1]));
    try testing.expectEqualSlices(u32, &.{9}, c.drainBatch(.unbounded, &out));
    try testing.expectEqual(@as(usize, 0), c.pending());
    try testing.expectEqual(@as(usize, 0), c.drainBatch(.unbounded, &out).len);
}

test "leadership_is_exclusive_and_recheck_reelects" {
    var slots: [4]Core.Entry = undefined;
    var c: Core = .init(testing.io, &slots, null);
    var out: [4]u32 = undefined;
    try testing.expect(c.tryLead()); // first elector wins
    try testing.expect(!c.tryLead()); // second elector loses to a live leader

    // Empty queue: release stands, no re-election.
    try testing.expect(!c.unleadAndRecheck());
    try testing.expect(c.tryLead()); // leadership was released

    // Non-empty queue: the leader must re-elect itself on release.
    try c.enqueue(7, 1);
    try testing.expect(c.unleadAndRecheck());
    try testing.expectEqualSlices(u32, &.{7}, c.drainBatch(.unbounded, &out));
    try testing.expect(!c.unleadAndRecheck());
    try testing.expect(!c.leader.load(.seq_cst));
}

test "enqueue_many_is_fifo_and_counted" {
    var slots: [8]Core.Entry = undefined;
    var c: Core = .init(testing.io, &slots, null);
    var out: [8]u32 = undefined;
    try c.enqueue(1, 1);
    try testing.expectEqual(@as(usize, 3), try c.enqueueMany(&.{ .{ .item = 2, .len = 1 }, .{ .item = 3, .len = 1 }, .{ .item = 4, .len = 1 } }));
    try testing.expectEqual(@as(usize, 0), try c.enqueueMany(&.{})); // an empty group is a no-op
    try testing.expectEqual(@as(usize, 4), c.pending());
    try testing.expectEqualSlices(u32, &.{ 1, 2, 3, 4 }, c.drainBatch(.unbounded, &out));
    try testing.expectEqual(@as(usize, 2), try c.enqueueMany(&.{ .{ .item = 5, .len = 10 }, .{ .item = 6, .len = 10 } }));
    // A byte cap below the group still drains it, one entry per drain
    // (first-entry progress) — grouping never bypasses a cap.
    try testing.expectEqualSlices(u32, &.{5}, c.drainBatch(.{ .max_txs = 64, .max_bytes = 5 }, &out));
    try testing.expectEqualSlices(u32, &.{6}, c.drainBatch(.{ .max_txs = 64, .max_bytes = 5 }, &out));
    try testing.expectEqual(@as(usize, 0), c.pending());
}

test "a full ring is a typed refusal; a group that does not fit enqueues nothing; the ring wraps" {
    var slots: [3]Core.Entry = undefined;
    var c: Core = .init(testing.io, &slots, null);
    var out: [3]u32 = undefined;
    try c.enqueue(1, 1);
    try c.enqueue(2, 1);
    try testing.expectError(error.Full, c.enqueueMany(&.{ .{ .item = 3, .len = 1 }, .{ .item = 4, .len = 1 } }));
    try testing.expectEqual(@as(usize, 2), c.pending());
    try c.enqueue(3, 1);
    try testing.expectError(error.Full, c.enqueue(4, 1));
    try testing.expectEqualSlices(u32, &.{ 1, 2 }, c.drainBatch(.{ .max_txs = 2, .max_bytes = no_byte_cap }, &out));
    // Wrap: head is at slot 2, the next pushes land in slots 0 and 1.
    try c.enqueue(4, 1);
    try c.enqueue(5, 1);
    try testing.expectEqualSlices(u32, &.{ 3, 4, 5 }, c.drainBatch(.unbounded, &out));
}

test "the queued gauge moves by exactly the enqueued and drained populations" {
    var gauge: counters.Gauge = .{};
    var slots: [8]Core.Entry = undefined;
    var c: Core = .init(testing.io, &slots, &gauge);
    var out: [8]u32 = undefined;
    try c.enqueue(1, 1);
    _ = try c.enqueueMany(&.{ .{ .item = 2, .len = 1 }, .{ .item = 3, .len = 1 } });
    try testing.expectEqual(@as(u64, 3), gauge.load());
    _ = c.drainBatch(.{ .max_txs = 2, .max_bytes = no_byte_cap }, &out);
    try testing.expectEqual(@as(u64, 1), gauge.load());
    _ = c.drainBatch(.unbounded, &out);
    try testing.expectEqual(@as(u64, 0), gauge.load());
    // A refused group and an empty drain leave the gauge alone.
    var tiny_slots: [1]Core.Entry = undefined;
    var tiny: Core = .init(testing.io, &tiny_slots, &gauge);
    try testing.expectError(error.Full, tiny.enqueueMany(&.{ .{ .item = 1, .len = 1 }, .{ .item = 2, .len = 1 } }));
    _ = tiny.drainBatch(.unbounded, &out);
    try testing.expectEqual(@as(u64, 0), gauge.load());
}

// Real threads: every committer enqueues then elects; whoever wins runs the
// pass loop. Every entry drains exactly once and nobody is left leading —
// the no-lost-wakeup theorem on hardware, the schedule left to the OS.
test "committer storm: every entry drains exactly once, leadership is released at quiescence" {
    const threads = 4;
    const per_thread: u32 = 500;
    var slots: [threads * per_thread]Core.Entry = undefined;
    var gauge: counters.Gauge = .{};
    var c: Core = .init(testing.io, &slots, &gauge);
    var drained: std.atomic.Value(u64) = .init(0);
    var budget: std.atomic.Value(u64) = .init(0);

    const Committer = struct {
        fn run(core: *Core, drained_total: *std.atomic.Value(u64), budget_total: *std.atomic.Value(u64)) void {
            var out: [64]u32 = undefined;
            var i: u32 = 0;
            while (i < per_thread) : (i += 1) {
                core.enqueue(i, i) catch unreachable; // capacity = the whole storm
                if (!core.tryLead()) continue;
                while (true) {
                    const batch = core.drainBatch(.{ .max_txs = 64, .max_bytes = no_byte_cap }, &out);
                    if (batch.len == 0) {
                        if (!core.unleadAndRecheck()) break;
                        continue;
                    }
                    _ = drained_total.fetchAdd(batch.len, .monotonic);
                    for (batch) |v| _ = budget_total.fetchAdd(v, .monotonic);
                }
            }
        }
    };
    var handles: [threads]std.Thread = undefined;
    for (&handles) |*h| h.* = try std.Thread.spawn(.{}, Committer.run, .{ &c, &drained, &budget });
    for (handles) |h| h.join();

    try testing.expectEqual(@as(u64, threads * per_thread), drained.load(.monotonic));
    try testing.expectEqual(@as(u64, threads * (per_thread * (per_thread - 1) / 2)), budget.load(.monotonic));
    try testing.expectEqual(@as(usize, 0), c.pending());
    try testing.expectEqual(@as(u64, 0), gauge.load());
    try testing.expect(!c.leader.load(.seq_cst));
}

test {
    _ = @import("conveyor_core_model.zig");
}
