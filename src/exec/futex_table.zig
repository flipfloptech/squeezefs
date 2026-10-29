//! The fiber-aware futex (`docs/porting/exec.md` §3.5): how every primitive
//! in `src/sync/` parks a FIBER, not the lane thread, under `SqzIo`.
//!
//! `src/sync/` waits through `tick.park` → `io.futexWaitTimeout` and its
//! interior `Io.Mutex` through `futexWait/futexWake`. On a lane fiber the
//! wait registers the fiber in a bucket list (256 buckets, each a spin lock
//! and an intrusive list of fibers keyed by the word's address), re-reads
//! the word under the bucket lock (a waker's store precedes its bucket
//! scan, so a waiter either sees the new word or is linked when the waker
//! looks), parks, and on resume unlinks itself if still linked (a timeout
//! or a cancel woke it). Off a lane the wait is the raw `futex(2)` with the
//! kernel's own timeout.
//!
//! The raw-waiter Dekker: a raw waiter increments its bucket's `raw_waiters`
//! BEFORE its kernel compare-and-wait; the waker stores the word (the
//! primitive's token) BEFORE it reads the count — both as RMWs on the count
//! word, so one of them observes the other: either the waker pays the kernel
//! wake or the raw waiter's compare sees the new word (`futex_table_model.zig`;
//! the weakening is counting after the wait). A kernel `FUTEX_WAKE` is paid
//! only when a raw waiter may exist in the word's bucket: the common
//! all-fiber case costs no syscall per wake, and a raw park elsewhere (a
//! journal lane's counted ticket wait) does not tax the handler lanes' wakes.
//!
//! The raw side is woken for the WHOLE count whatever the fibers took of it
//! (`DrivenOwnerModel`): a counted raw waiter may be the owner thread of a
//! DRIVEN lane a woken fiber lives on — the FUSE drain worker, off a fiber
//! in a try-only probe, behind one of its own handler fibers on a mutex's
//! interior word. That fiber runs only once the owner is back in its pass,
//! so a count-one wake the fiber took alone left both asleep on a FREE word
//! (`docs/evidence/rung-10-mdtest-wedge.md`). A spurious raw wake costs one
//! re-contend: every caller loops on its predicate.
//!
//! Linux only.

const std = @import("std");
const builtin = @import("builtin");
const Io = std.Io;
const assert = std.debug.assert;
const futex = @import("../sync/futex.zig");
const fiber_mod = @import("fiber.zig");
const stats = @import("stats.zig");

comptime {
    if (builtin.os.tag != .linux) @compileError("exec/futex_table is Linux only (charter §1.2); gate the import on builtin.os.tag");
}

pub const Fiber = fiber_mod.Fiber;

/// 256 buckets: the hot words (one interior mutex and one waiter node per
/// parked primitive, a few dozen live at once) spread so two unrelated
/// primitives rarely share a lock; a collision costs one spin, never
/// correctness. 256 × 24 B = 6 KiB.
pub const bucket_count = 256;

/// The fibers one hold of a bucket lock takes for a wake: two cache lines of
/// pointers on the waker's frame. The common wake is one (a mutex's
/// release); a broadcast to more waiters on one word takes another pass.
pub const wake_batch = 16;

const SpinLock = struct {
    locked: std.atomic.Value(bool) = .init(false),

    fn acquire(self: *SpinLock) void {
        // acquire pairs with `release`'s release store: the list is visible.
        while (self.locked.swap(true, .acquire)) std.atomic.spinLoopHint();
    }
    fn release(self: *SpinLock) void {
        self.locked.store(false, .release);
    }
};

const Bucket = struct {
    lock: SpinLock = .{},
    head: ?*Fiber = null,
    tail: ?*Fiber = null,
    /// Raw (non-lane) threads inside a kernel wait on a word of this
    /// bucket, or about to enter one (the Dekker's count).
    raw_waiters: std.atomic.Value(u32) = .init(0),

    fn link(b: *Bucket, f: *Fiber) void {
        assert(!f.futex_linked);
        f.futex_next = null;
        f.futex_prev = b.tail;
        if (b.tail) |t| t.futex_next = f else b.head = f;
        b.tail = f;
        // release pairs with `unlinkIfLinked`'s acquire load outside the lock.
        @atomicStore(bool, &f.futex_linked, true, .release);
    }

    fn unlink(b: *Bucket, f: *Fiber) void {
        assert(f.futex_linked);
        if (f.futex_prev) |p| p.futex_next = f.futex_next else b.head = f.futex_next;
        if (f.futex_next) |n| n.futex_prev = f.futex_prev else b.tail = f.futex_prev;
        f.futex_next = null;
        f.futex_prev = null;
        @atomicStore(bool, &f.futex_linked, false, .release);
    }
};

pub const FutexTable = struct {
    buckets: [bucket_count]Bucket = @splat(.{}),
    /// Test seam: runs inside `wake` right after the bucket lock's release
    /// — the window in which a fiber a stray wake resumed re-parks.
    test_after_scan: if (builtin.is_test) ?TestHook else void = if (builtin.is_test) null else {},

    pub const init: FutexTable = .{};

    pub const TestHook = struct {
        ctx: *anyopaque,
        call: *const fn (ctx: *anyopaque) void,
    };

    fn bucketOf(self: *FutexTable, ptr: *const u32) *Bucket {
        // Fibonacci hashing of the word address (4-byte granules).
        const h = (@as(u64, @intFromPtr(ptr)) >> 2) *% 0x9E3779B97F4A7C15;
        return &self.buckets[@intCast(h >> (64 - 8))];
    }

    /// The fiber path. `deadline_ns` is monotonic (null = unbounded).
    /// `cancelable` makes this a cancellation point: a request that lands
    /// before or during the park returns `error.Canceled` once.
    pub fn fiberWait(self: *FutexTable, f: *Fiber, ptr: *const u32, expected: u32, deadline_ns: ?u64, comptime cancelable: bool) Io.Cancelable!void {
        if (cancelable) try f.checkCancel();
        const b = self.bucketOf(ptr);
        b.lock.acquire();
        // The re-read under the lock: the token stored before the waker's
        // scan is visible here (seq_cst load vs the primitive's release
        // store + the waker's bucket acquire).
        if (@atomicLoad(u32, ptr, .seq_cst) != expected) {
            b.lock.release();
            return;
        }
        f.futex_ptr = ptr;
        b.link(f);
        b.lock.release();
        // Registered: from here a cancel request wakes us.
        if (f.changeAwaiting(.nothing, .wakeable) and cancelable and f.protection.check() == .unblocked) {
            self.unlinkIfLinked(f);
            _ = f.changeAwaiting(.wakeable, .nothing);
            f.protection.acknowledged = true;
            return error.Canceled;
        }
        if (deadline_ns) |d| {
            f.lane.timers.arm(f, d);
            stats.timer_arms.inc();
        }
        f.park();
        if (deadline_ns != null) _ = f.lane.timers.cancel(f);
        self.unlinkIfLinked(f);
        const requested = f.changeAwaiting(.wakeable, .nothing);
        if (cancelable and requested and f.protection.check() == .unblocked) {
            f.protection.acknowledged = true;
            return error.Canceled;
        }
    }

    /// A timeout or a cancel woke the fiber while a waker had not unlinked
    /// it: take the bucket lock and leave.
    fn unlinkIfLinked(self: *FutexTable, f: *Fiber) void {
        // acquire pairs with the waker's release (the lock release after
        // its unlink): a false here means a waker already took us, and it
        // never touches our link again (its late `state.wake()` is at most
        // a spurious wake of our next park).
        if (!@atomicLoad(bool, &f.futex_linked, .acquire)) return;
        const b = self.bucketOf(f.futex_ptr.?);
        b.lock.acquire();
        if (f.futex_linked) b.unlink(f);
        b.lock.release();
    }

    /// The raw-thread path: counted in the word's bucket, then the kernel
    /// wait (`timeout_ns` relative, null = unbounded). seq_cst RMWs on the
    /// count pair with `wake`'s read (the Dekker, module doc).
    pub fn rawWait(self: *FutexTable, ptr: *const u32, expected: u32, timeout_ns: ?u64) void {
        const b = self.bucketOf(ptr);
        _ = b.raw_waiters.fetchAdd(1, .seq_cst);
        defer _ = b.raw_waiters.fetchSub(1, .seq_cst);
        const word: *const futex.Word = @ptrCast(@alignCast(ptr));
        if (timeout_ns) |ns| {
            _ = futex.waitTimeout(word, expected, ns);
        } else {
            futex.wait(word, expected);
        }
    }

    /// Wake up to `max` waiters on `ptr`: matching fibers (moved off the
    /// list under the lock into a stack batch, woken outside it — the
    /// enqueue may write an eventfd), AND up to `max` raw waiters through
    /// the kernel iff the bucket counts any — never the remainder alone
    /// (module doc: a counted raw waiter may be what runs the fibers just
    /// woken).
    ///
    /// No fiber's link is touched after the lock: a fiber a stray wake (a
    /// timer pop, a late waker) already resumed may re-park in any bucket
    /// the moment the lock is free, and a write to its link then tears that
    /// bucket's list and strands the waiter queued behind it
    /// (`docs/evidence/rung-10-journal-full-wedge.md`, the pass-recall
    /// wedge). The late `state.wake()` such a fiber may still meet is a
    /// spurious wake of its next park — every park loops on its predicate,
    /// and `fiberWait`'s resume unlinks itself (`futex_table_model.zig`
    /// `ChainWalkModel`). The lock covers no delivery-word RMW.
    ///
    /// More matches than one batch: the scan counts the matches behind the
    /// batch (up to what the wake still owes) and the next pass, from the
    /// head, owes only those — a fiber that re-parks behind them on the
    /// same word is not chased.
    pub fn wake(self: *FutexTable, ptr: *const u32, max: u32) void {
        if (max == 0) return;
        const b = self.bucketOf(ptr);
        var remaining = max;
        var batch: [wake_batch]*Fiber = undefined;
        while (remaining > 0) {
            var n: u32 = 0;
            var left: u32 = 0;
            b.lock.acquire();
            var cur = b.head;
            while (cur) |f| {
                cur = f.futex_next;
                if (f.futex_ptr != ptr) continue;
                if (n < wake_batch) {
                    b.unlink(f);
                    batch[n] = f;
                    n += 1;
                } else {
                    left += 1;
                }
                if (n + left == remaining) break;
            }
            b.lock.release();
            if (builtin.is_test) if (self.test_after_scan) |h| h.call(h.ctx);
            for (batch[0..n]) |f| {
                if (f.state.wake()) f.lane.enqueue(f);
            }
            remaining = left;
        }
        // The Dekker read: a seq_cst RMW (fetchAdd 0) is the fence AND the
        // load — it is totally ordered against every raw waiter's increment.
        if (b.raw_waiters.fetchAdd(0, .seq_cst) != 0) {
            futex.wake(@ptrCast(@alignCast(ptr)), max);
        }
    }

    /// Raw waiters counted right now in `ptr`'s bucket (tests).
    pub fn rawWaiters(self: *FutexTable, ptr: *const u32) u32 {
        return self.bucketOf(ptr).raw_waiters.load(.seq_cst);
    }
};

const testing = std.testing;

test "raw waiters: a wake from another thread ends a counted wait; expected mismatch returns at once; a timed wait expires; the count is the word's bucket's and returns to 0" {
    var t: FutexTable = .init;
    var word: futex.Word = .init(0);
    t.rawWait(&word.raw, 1, null);
    try testing.expectEqual(@as(u32, 0), t.rawWaiters(&word.raw));
    const Waker = struct {
        fn run(tab: *FutexTable, w: *futex.Word) void {
            while (tab.rawWaiters(&w.raw) == 0) std.Thread.yield() catch {};
            w.store(1, .release);
            tab.wake(&w.raw, 1);
        }
    };
    const th = try std.Thread.spawn(.{}, Waker.run, .{ &t, &word });
    while (word.load(.acquire) == 0) t.rawWait(&word.raw, 0, null);
    th.join();
    try testing.expectEqual(@as(u32, 0), t.rawWaiters(&word.raw));
    word.store(0, .monotonic);
    t.rawWait(&word.raw, 0, 2 * std.time.ns_per_ms);
    try testing.expectEqual(@as(u32, 0), t.rawWaiters(&word.raw));
    // A wake with no waiters pays nothing and is harmless.
    t.wake(&word.raw, std.math.maxInt(u32));
    t.wake(&word.raw, 0);
}

test "the raw count is per bucket: a raw waiter on one word is not counted against a word of another bucket" {
    var t: FutexTable = .init;
    // Two words whose addresses hash to different buckets (the first pair
    // of a page-spread that does; 256 buckets, Fibonacci hashing).
    var words: [64]futex.Word = @splat(.init(0));
    const a: usize = 0;
    var b: usize = 1;
    while (b < words.len and t.bucketOf(&words[a].raw) == t.bucketOf(&words[b].raw)) b += 1;
    try testing.expect(b < words.len);
    const Sleeper = struct {
        fn run(tab: *FutexTable, w: *futex.Word) void {
            tab.rawWait(&w.raw, 0, null);
        }
    };
    const th = try std.Thread.spawn(.{}, Sleeper.run, .{ &t, &words[a] });
    while (t.rawWaiters(&words[a].raw) == 0) try std.Thread.yield();
    try testing.expectEqual(@as(u32, 0), t.rawWaiters(&words[b].raw));
    words[a].store(1, .release);
    t.wake(&words[a].raw, 1);
    th.join();
    try testing.expectEqual(@as(u32, 0), t.rawWaiters(&words[a].raw));
}

/// What `fiberWait` does before its park: link under the bucket lock.
fn testLink(t: *FutexTable, f: *Fiber, ptr: *const u32) void {
    const b = t.bucketOf(ptr);
    b.lock.acquire();
    f.futex_ptr = ptr;
    b.link(f);
    b.lock.release();
}

/// The forward walk from `head` reaches `tail` with every `futex_prev` its
/// predecessor: the shape every scan relies on.
fn testShapeHolds(b: *Bucket) bool {
    var prev: ?*Fiber = null;
    var cur = b.head;
    var n: usize = 0;
    while (cur) |f| : (cur = f.futex_next) {
        if (f.futex_prev != prev or !f.futex_linked) return false;
        prev = f;
        n += 1;
        if (n > 64) return false;
    }
    return prev == b.tail;
}

/// A slot that never reaches a lane: every wake below finds it running or
/// notified, so `state.wake()` never asks for an enqueue.
fn testFiber(f: *Fiber) void {
    f.stack = null;
    f.idle_tick = 0;
    f.reset(undefined, 0);
    assert(f.state.beginPoll());
}

test "a wake never touches a link after the bucket lock: a fiber a stray wake resumed re-parks while the wake is still in flight, and the waiter queued behind it stays reachable (the pass-recall wedge)" {
    var t: FutexTable = .init;
    var p: futex.Word = .init(0);
    var q: futex.Word = .init(0);
    var f: Fiber = undefined;
    var g: Fiber = undefined;
    testFiber(&f);
    testFiber(&g);
    // f parked on p; a stray wake (a timer pop, a late waker — the futex
    // contract allows it) resumed it: it is running toward its own unlink
    // when p's release scans the bucket and takes it.
    testLink(&t, &f, &p.raw);
    const Window = struct {
        t: *FutexTable,
        f: *Fiber,
        g: *Fiber,
        q: *const u32,
        fn run(ctx: *anyopaque) void {
            const w: *@This() = @ptrCast(@alignCast(ctx));
            // f leaves its wait (the waker already unlinked it) and parks
            // on q; g parks on q behind it (the pass fiber on a lock's
            // interior word in the field).
            w.t.unlinkIfLinked(w.f);
            testLink(w.t, w.f, w.q);
            testLink(w.t, w.g, w.q);
        }
    };
    var win: Window = .{ .t = &t, .f = &f, .g = &g, .q = &q.raw };
    t.test_after_scan = .{ .ctx = &win, .call = Window.run };
    t.wake(&p.raw, 1);
    t.test_after_scan = null;
    const shape_held = testShapeHolds(t.bucketOf(&q.raw));
    // q's release must reach both waiters.
    q.store(1, .release);
    t.wake(&q.raw, std.math.maxInt(u32));
    try testing.expect(!f.futex_linked);
    try testing.expect(!g.futex_linked);
    try testing.expect(shape_held);
    try testing.expect(testShapeHolds(t.bucketOf(&q.raw)));
}

test "a wake-all over more waiters than one batch: every waiter present is taken once, in queue order, over three passes; the bucket's other word keeps its waiters and its shape; a fiber that parks on the word behind them during the wake is not chased" {
    var t: FutexTable = .init;
    // A word and a neighbour that share its bucket.
    var words: [4096]futex.Word = @splat(.init(0));
    var o: usize = 1;
    while (o < words.len and t.bucketOf(&words[0].raw) != t.bucketOf(&words[o].raw)) o += 1;
    try testing.expect(o < words.len);
    const word = &words[0].raw;
    const b = t.bucketOf(word);
    const waiters = 2 * wake_batch + 5;
    const fibers = try testing.allocator.alloc(Fiber, waiters + 4);
    defer testing.allocator.free(fibers);
    for (fibers) |*f| testFiber(f);
    const others = fibers[waiters..][0..3];
    const late = &fibers[waiters + 3];
    for (fibers[0..waiters], 0..) |*f, i| {
        if (i % 13 == 0) testLink(&t, &others[i / 13], &words[o].raw);
        testLink(&t, f, word);
    }
    const Passes = struct {
        t: *FutexTable,
        waiters: []Fiber,
        late: *Fiber,
        word: *const u32,
        /// Waiters off the list at each pass's release, in queue order.
        taken: [4]usize = @splat(0),
        prefix: [4]bool = @splat(false),
        n: usize = 0,
        fn run(ctx: *anyopaque) void {
            const p: *@This() = @ptrCast(@alignCast(ctx));
            var off: usize = 0;
            while (off < p.waiters.len and !p.waiters[off].futex_linked) off += 1;
            var prefix = true;
            for (p.waiters[off..]) |*f| prefix = prefix and f.futex_linked;
            if (p.n < p.taken.len) {
                p.taken[p.n] = off;
                p.prefix[p.n] = prefix;
            }
            // A fiber parks on the word while the wake is in flight.
            if (p.n == 0) testLink(p.t, p.late, p.word);
            p.n += 1;
        }
    };
    var passes: Passes = .{ .t = &t, .waiters = fibers[0..waiters], .late = late, .word = word };
    t.test_after_scan = .{ .ctx = &passes, .call = Passes.run };
    words[0].store(1, .release);
    t.wake(word, std.math.maxInt(u32));
    t.test_after_scan = null;
    try testing.expectEqual(@as(usize, 3), passes.n);
    try testing.expectEqualSlices(usize, &.{ wake_batch, 2 * wake_batch, waiters, 0 }, &passes.taken);
    try testing.expectEqualSlices(bool, &.{ true, true, true, false }, &passes.prefix);
    // Each was woken (running: the wake left it NOTIFIED) and is off the list.
    for (fibers[0..waiters]) |*f| {
        try testing.expect(!f.futex_linked);
        try testing.expect(f.state.load() == .notified);
    }
    for (others) |*f| try testing.expect(f.futex_linked);
    try testing.expect(late.futex_linked);
    try testing.expect(late.state.load() == .running);
    try testing.expect(testShapeHolds(b));
    var linked: usize = 0;
    var cur = b.head;
    while (cur) |f| : (cur = f.futex_next) linked += 1;
    try testing.expectEqual(@as(usize, others.len + 1), linked);
}
