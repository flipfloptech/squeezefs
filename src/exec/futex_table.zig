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

    pub const init: FutexTable = .{};

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
        // its unlink): a false here means the waker already moved us.
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
    /// list under the lock, woken outside it — the enqueue may write an
    /// eventfd), AND up to `max` raw waiters through the kernel iff the
    /// bucket counts any — never the remainder alone (module doc: a counted
    /// raw waiter may be what runs the fibers just woken).
    pub fn wake(self: *FutexTable, ptr: *const u32, max: u32) void {
        if (max == 0) return;
        var remaining = max;
        var chain: ?*Fiber = null;
        const b = self.bucketOf(ptr);
        b.lock.acquire();
        var cur = b.head;
        while (cur) |f| {
            const next = f.futex_next;
            if (f.futex_ptr == ptr) {
                b.unlink(f);
                f.futex_next = chain;
                chain = f;
                remaining -= 1;
                if (remaining == 0) break;
            }
            cur = next;
        }
        b.lock.release();
        while (chain) |f| {
            chain = f.futex_next;
            f.futex_next = null;
            if (f.state.wake()) f.lane.enqueue(f);
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
